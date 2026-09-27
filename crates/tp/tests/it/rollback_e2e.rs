//! `fl rollback` puts back what the last install replaced.
//!
//! Driven through the real binary with a fake HOME: the thing under test is a
//! filesystem swap in `~/.local/bin`, and the two failures that matter —
//! rolling back with nothing behind you, and rolling back leaving the pair
//! mismatched — live in the command, not in the rename logic.

use std::process::Command;

fn bin_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".local").join("bin")
}

/// A "binary" that reports a version, so the swap can be observed by running it.
fn fake_binary(path: &std::path::Path, version: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_rollback(home: &std::path::Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .arg("rollback")
        .env("HOME", home)
        .output()
        .unwrap();
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

#[test]
fn a_first_install_has_nothing_behind_it_and_says_so() {
    // Undoing a first install has nothing to undo. "No such file" is true and
    // useless; naming the situation and where to get an older release is the answer.
    let tmp = tempfile::tempdir().unwrap();
    let bin = bin_dir(tmp.path());
    std::fs::create_dir_all(&bin).unwrap();
    fake_binary(&bin.join("fl"), "fl 9.9.9");
    fake_binary(&bin.join("fld"), "fld 9.9.9");

    let (ok, out) = run_rollback(tmp.path());
    assert!(!ok, "must refuse, not silently do nothing:\n{out}");
    assert!(out.contains("no previous build"), "{out}");
    assert!(out.contains("releases"), "must say where to get one: {out}");
}

#[test]
fn a_half_present_pair_is_refused_before_anything_moves() {
    // fl and fld share a schema, so a half-restored pair is worse than either
    // version. The check precedes the first rename, or a refusal splits the pair.
    let tmp = tempfile::tempdir().unwrap();
    let bin = bin_dir(tmp.path());
    std::fs::create_dir_all(&bin).unwrap();
    fake_binary(&bin.join("fl"), "fl new");
    fake_binary(&bin.join("fld"), "fld new");
    fake_binary(&bin.join(".fl.prev"), "fl old");
    // .fld.prev deliberately absent.

    let (ok, out) = run_rollback(tmp.path());
    assert!(!ok, "{out}");
    assert!(
        std::fs::read_to_string(bin.join("fl"))
            .unwrap()
            .contains("fl new"),
        "nothing may move when the pair is incomplete"
    );
    assert!(
        bin.join(".fl.prev").exists(),
        "and the copy must survive too"
    );
}

#[test]
fn rollback_swaps_and_is_itself_reversible() {
    // Swap, not overwrite: running it twice returns to the start rather than
    // stranding the user one version back with nothing ahead.
    let tmp = tempfile::tempdir().unwrap();
    let bin = bin_dir(tmp.path());
    std::fs::create_dir_all(&bin).unwrap();
    fake_binary(&bin.join("fl"), "fl 0.2.1");
    fake_binary(&bin.join("fld"), "fld 0.2.1");
    fake_binary(&bin.join(".fl.prev"), "fl 0.2.0");
    fake_binary(&bin.join(".fld.prev"), "fld 0.2.0");

    let (ok, out) = run_rollback(tmp.path());
    assert!(ok, "{out}");
    let live = std::fs::read_to_string(bin.join("fl")).unwrap();
    assert!(
        live.contains("fl 0.2.0"),
        "live binary must be the old one: {live}"
    );
    let kept = std::fs::read_to_string(bin.join(".fl.prev")).unwrap();
    assert!(
        kept.contains("fl 0.2.1"),
        "and the new one must be kept: {kept}"
    );
    assert!(
        out.contains("rolled back to") && out.contains("0.2.0"),
        "it must report the version it landed on, not its own: {out}"
    );
    assert!(
        out.contains("launchctl kickstart"),
        "a LaunchAgent keeps running what it started with; saying so is the point: {out}"
    );

    let (ok, _) = run_rollback(tmp.path());
    assert!(ok);
    let live = std::fs::read_to_string(bin.join("fl")).unwrap();
    assert!(
        live.contains("fl 0.2.1"),
        "twice must return to the start: {live}"
    );
}
