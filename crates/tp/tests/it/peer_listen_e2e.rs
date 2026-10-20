//! The peer port is off until someone says otherwise.
//!
//! This is the one default in floonet that puts a socket on a stranger's
//! machine, so it is the one that must not drift quietly: a listener is the
//! only thing that makes an install reachable from outside, and "security is
//! the user's to add" cannot cover a default the user did not add.
//!
//! Run against the real binary: what ships is the CLI's behaviour, and a
//! library-level assertion would still pass if `main` stopped consulting the setting.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fl(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .args(args)
        .env("HOME", home)
        // The escape hatch must not leak in from the developer's own shell and
        // make a configured port look like it was honoured.
        .env_remove("TP_PORT")
        .output()
        .expect("run fl");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

#[test]
fn a_fresh_install_does_not_listen_and_says_what_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, out) = fl(dir.path(), &["listen"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("NOT listening"),
        "a machine nobody configured must not be reachable: {out}"
    );
    // Saying only "off" reads as "broken" to someone who came here because a
    // paired machine went quiet. The state has to arrive with its scope.
    assert!(
        out.contains("mailbox") && out.contains("search"),
        "the off state must say what is unaffected: {out}"
    );
    assert!(
        out.contains("47400") && out.contains("default"),
        "and which port it would use if turned on: {out}"
    );
}

#[test]
fn turning_it_on_is_remembered_with_its_port() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, out) = fl(dir.path(), &["listen", "on", "--port", "47500"]);
    assert!(ok, "{out}");
    assert!(out.contains("ENABLED"), "{out}");
    // Nothing rebinds a port under a running server, so a caller told only
    // "enabled" would believe a machine was reachable that is not yet.
    assert!(
        out.to_lowercase().contains("restart"),
        "must say the change needs a daemon restart: {out}"
    );

    let (ok, out) = fl(dir.path(), &["listen"]);
    assert!(ok, "{out}");
    assert!(out.contains("listening on port 47500"), "{out}");
    assert!(
        out.contains("configured"),
        "a chosen port is not the default: {out}"
    );
}

#[test]
fn turning_it_off_again_returns_to_not_listening() {
    let dir = tempfile::tempdir().unwrap();
    fl(dir.path(), &["listen", "on"]);
    let (ok, out) = fl(dir.path(), &["listen", "off"]);
    assert!(ok, "{out}");
    assert!(out.contains("disabled"), "{out}");

    let (ok, out) = fl(dir.path(), &["listen"]);
    assert!(ok, "{out}");
    assert!(out.contains("NOT listening"), "{out}");
}

#[test]
fn the_environment_overrides_a_configured_port() {
    let dir = tempfile::tempdir().unwrap();
    fl(dir.path(), &["listen", "on", "--port", "47500"]);
    let out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .args(["listen"])
        .env("HOME", dir.path())
        .env("TP_PORT", "47600")
        .output()
        .expect("run fl");
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        s.contains("47600"),
        "TP_PORT is the escape hatch for a second daemon and must win: {s}"
    );
}

/// The default lives in one expression, and it is `false`.
///
/// A grep-shaped guard, because the type system cannot express it: `enabled`
/// is a plain `bool` read out of a `setting` row, and an edit that seeds a row,
/// flips the fallback, or adds an `unwrap_or(true)` compiles fine and changes
/// what a stranger's machine does on first run.
#[test]
fn nothing_makes_the_peer_port_default_to_on() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");

    let app = std::fs::read_to_string(root.join("crates/tp-app/src/app.rs")).unwrap();
    assert!(
        app.contains(r#"is_some_and(|v| v == "1")"#),
        "peer_listen must resolve absent-or-unreadable to OFF by construction"
    );
    assert!(
        !app.contains("unwrap_or(true)"),
        "an `unwrap_or(true)` anywhere near this setting is the default flipping"
    );

    // A migration that INSERTs a row would make "the user chose" and "floonet
    // decided" indistinguishable.
    let mig =
        std::fs::read_to_string(root.join("crates/tp-db/migrations/0018_setting.sql")).unwrap();
    let sql = mig.to_uppercase();
    assert!(
        !sql.contains("INSERT"),
        "0018 must not seed a row: a seeded value is indistinguishable from a chosen one"
    );
}

/// Closing the peer port must not close anything else.
///
/// A grep-shaped guard on `fld`'s startup order, because it cannot be reached
/// from a test that does not run a daemon. The disabled branch decides only
/// whether to bind; an early `return` or `run_forever` inside it would skip the
/// discovery scan and everything after it on the default configuration.
#[test]
fn closing_the_peer_port_does_not_skip_the_rest_of_startup() {
    let src =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/bin/fld.rs"))
            .unwrap();

    let spawn = src
        .find("run_discovery_scan")
        .expect("fld must spawn the discovery scan");
    let branch = src
        .find("if listen.enabled")
        .expect("fld must consult the peer-listen setting");
    assert!(
        branch < spawn,
        "the listen branch must come BEFORE the scan spawn, so that skipping the \
         bind cannot skip the scan"
    );

    // Between the branch and the spawn there must be no early exit.
    let between = &src[branch..spawn];
    for bad in ["return Ok(())", "run_forever"] {
        assert!(
            !between.contains(bad),
            "`{bad}` between the listen branch and the scan spawn — the daemon \
             would stop starting once the port is off, which is the default"
        );
    }
}
