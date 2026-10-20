//! `fld` must stop cleanly when launchd stops it.
//!
//! launchd sends SIGTERM and, after its exit timeout, SIGKILL. The plist asks
//! for a restart only on an UNCLEAN exit (`KeepAlive` / `SuccessfulExit`
//! false), because a clean exit is an intentional stop and respawning it would
//! fight the operator. A process terminated by a signal is not a clean exit, so
//! a daemon that leaves SIGTERM at its default disposition asks launchd for
//! exactly the respawn the plist exists to prevent.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Spawn `fld` against a private HOME.
///
/// `env_clear` for the reason `reply_hint_e2e` records: the harness running the
/// suite exports session variables that change what the binary does. HOME is
/// what isolates this, and it has to be HOME rather than `TP_DB`, because
/// `tp_net::identity::default_key_path` reads HOME directly and would otherwise
/// create a key in the developer's own `~/.teleport`. PATH is needed because
/// the discovery scan spawns `ps`.
fn spawn_fld(home: &Path, log: &Path) -> Child {
    let err = std::fs::File::create(log).expect("create fld log");
    Command::new(env!("CARGO_BIN_EXE_fld"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn fld")
}

fn read_log(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
fn sigterm_stops_the_daemon_with_a_clean_exit() {
    let home = tempfile::tempdir().unwrap();
    let log = home.path().join("fld.stderr");
    let mut child = spawn_fld(home.path(), &log);

    // Wait for the socket rather than sleeping: the local adapter binds it just
    // before the daemon parks on the signal wait, so its appearance is the
    // signal that there is something to interrupt.
    let sock = home.path().join(".teleport").join("tpd.sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !sock.exists() {
        if let Some(status) = child.try_wait().expect("wait fld") {
            panic!("fld exited before binding ({status}):\n{}", read_log(&log));
        }
        assert!(
            Instant::now() < deadline,
            "fld never bound {}:\n{}",
            sock.display(),
            read_log(&log)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let pid = child.id().to_string();
    let sent = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("send SIGTERM");
    assert!(sent.success(), "could not signal fld");

    // Ten seconds is inside launchd's own exit timeout, so a daemon that needs
    // longer than this is one launchd would SIGKILL in production.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("wait fld") {
            assert!(
                status.success(),
                "SIGTERM must end in a clean exit, got {status}:\n{}",
                read_log(&log)
            );
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "fld was still running 10s after SIGTERM:\n{}",
                read_log(&log)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
