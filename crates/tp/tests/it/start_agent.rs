//! `install/start-agent.sh`: starting fld's LaunchAgent.
//!
//! Driven with a stand-in `launchctl` that keeps its state in files, because
//! the real one would load a service into the machine running the tests. The
//! stand-in reproduces what was measured on a real machine over ssh: the
//! session reports `Background`, the user's `gui/<uid>` domain exists because
//! they are logged in at the screen, and a service left disabled by an earlier
//! run refuses to bootstrap with the same opaque error launchd gives.

use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "test.agent";

struct Machine {
    _tmp: tempfile::TempDir,
    bin: PathBuf,
    state: PathBuf,
    plist: PathBuf,
}

impl Machine {
    /// `gui` — the user is logged in at the screen, so `gui/<uid>` exists.
    /// `disabled` — an earlier run left the service disabled.
    fn new(gui: bool, disabled: bool) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        if gui {
            std::fs::write(state.join("gui"), "").unwrap();
        }
        if disabled {
            std::fs::write(state.join("disabled"), "").unwrap();
        }
        let plist = tmp.path().join("agent.plist");
        std::fs::write(&plist, "<plist/>").unwrap();

        // Each branch mirrors one observed behaviour of the real launchctl.
        let fake = format!(
            r#"#!/bin/sh
S='{state}'
echo "$*" >> "$S/calls"
case "$1" in
  managername) echo Background ;;
  print-disabled)
    if [ -f "$S/disabled" ]; then echo '	"{LABEL}" => disabled'; else echo '	"{LABEL}" => enabled'; fi ;;
  enable) rm -f "$S/disabled" ;;
  bootout) rm -f "$S/loaded" ;;
  bootstrap)
    if [ ! -f "$S/gui" ]; then echo "Bootstrap failed: 125: Domain does not support specified action" >&2; exit 125; fi
    if [ -f "$S/disabled" ]; then echo "Bootstrap failed: 5: Input/output error" >&2; exit 5; fi
    touch "$S/loaded" ;;
  print)
    case "$2" in
      */{LABEL}) [ -f "$S/loaded" ] ;;
      *) [ -f "$S/gui" ] ;;
    esac ;;
esac
"#,
            state = state.display()
        );
        let path = bin.join("launchctl");
        std::fs::write(&path, fake).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Machine {
            _tmp: tmp,
            bin,
            state,
            plist,
        }
    }

    fn start(&self) -> (i32, String) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/start-agent.sh");
        let out = Command::new("bash")
            .arg("-c")
            .arg(r#"source "$0"; start_agent "$1" "$2""#)
            .arg(&script)
            .arg(LABEL)
            .arg(&self.plist)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .output()
            .unwrap();
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.code().unwrap_or(-1), s)
    }

    fn running(&self) -> bool {
        self.state.join("loaded").exists()
    }
}

#[test]
fn an_ssh_session_starts_the_agent_when_the_user_is_logged_in() {
    // Measured, not assumed: over ssh `managername` says Background, and the
    // bootstrap into gui/<uid> still succeeds when that domain exists. The old
    // guard refused exactly this case and told the user to go to the screen.
    let m = Machine::new(true, false);
    let (code, out) = m.start();
    assert_eq!(code, 0, "{out}");
    assert!(m.running(), "the agent must be loaded: {out}");
}

#[test]
fn a_service_left_disabled_is_enabled_before_it_is_bootstrapped() {
    // The failure that looked like "ssh cannot do this": a disabled service
    // refuses to load, and launchd says only "Input/output error". The flag
    // survives uninstall and reboot, so a reinstall must clear it itself.
    let m = Machine::new(true, true);
    let (code, out) = m.start();
    assert_eq!(code, 0, "{out}");
    assert!(
        m.running(),
        "a stale disabled flag blocked the start: {out}"
    );
}

#[test]
fn with_no_one_logged_in_it_says_when_it_will_start_instead_of_failing() {
    // No gui/<uid> domain means nowhere to load into yet. The plist is in
    // LaunchAgents, so the next login starts it — that is a deferral, not an
    // install failure.
    let m = Machine::new(false, false);
    let (code, out) = m.start();
    assert_eq!(code, 2, "{out}");
    assert!(!m.running());
    assert!(
        out.contains("next login"),
        "must say when it will start: {out}"
    );
}

#[test]
fn a_real_failure_names_its_cause() {
    // Bootstrap can still fail with the domain present — here, disabled and
    // enabling it did not take. "Input/output error" alone sent one
    // investigation down the wrong path; the disabled state must be named.
    let m = Machine::new(true, true);
    // Sabotage `enable` so the flag survives.
    let fake = std::fs::read_to_string(m.bin.join("launchctl"))
        .unwrap()
        .replace(r#"enable) rm -f "$S/disabled" ;;"#, "enable) ;;");
    std::fs::write(m.bin.join("launchctl"), fake).unwrap();
    let (code, out) = m.start();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("disabled"), "the cause must be named: {out}");
}
