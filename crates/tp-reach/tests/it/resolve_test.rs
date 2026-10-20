//! Live-session resolution tests. The optimistic path (a real tmux or
//! terminal session) cannot run headless, so these assert the stable meanings
//! of the harder cases.

use tp_db::Db;
use tp_reach::{register, resolve, unregister, Target};

/// An alive pid guaranteed to have no controlling tty, whatever tty the test
/// process itself runs under. `std::process::id()` is not safe for that: with
/// a real terminal session at the runner's own tty, `resolve()` would return
/// `Target::Terminal`. A detached child with stdio piped away has none.
struct TtylessChild(std::process::Child);
impl TtylessChild {
    fn spawn() -> Self {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}
impl Drop for TtylessChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn no_binding_is_not_live() {
    let db = Db::open_in_memory().unwrap();
    let t = resolve(db.conn(), "m1/claude_code/nope").unwrap();
    assert_eq!(t, Target::NotLive);
}

#[test]
fn register_then_unregister_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    let child = TtylessChild::spawn();
    register(
        db.conn(),
        "m1/claude_code/sess1",
        child.pid(),
        Some("/dev/ttys000"),
        None,
    )
    .unwrap();
    // No pane and no terminal session for a tty-less pid: Unreachable, not
    // NotLive, because the pid is alive.
    let t = resolve(db.conn(), "m1/claude_code/sess1").unwrap();
    assert_eq!(
        t,
        Target::Unreachable,
        "alive pid + no tty must be Unreachable, not NotLive"
    );

    unregister(db.conn(), "m1/claude_code/sess1", None).unwrap();
    assert_eq!(
        resolve(db.conn(), "m1/claude_code/sess1").unwrap(),
        Target::NotLive
    );
}

#[test]
fn unregister_is_a_noop_when_the_session_id_was_reclaimed_by_a_different_pid() {
    // session_id reuse across `/clear`: SessionEnd for the old process races
    // SessionStart for the new one, and unregistering by session_id alone
    // would delete the newer, still-live binding.
    let db = Db::open_in_memory().unwrap();
    let old_child = TtylessChild::spawn();
    let new_child = TtylessChild::spawn();

    register(
        db.conn(),
        "m1/claude_code/reused",
        old_child.pid(),
        None,
        None,
    )
    .unwrap();
    // The new session's SessionStart lands first and reclaims the same id.
    register(
        db.conn(),
        "m1/claude_code/reused",
        new_child.pid(),
        None,
        None,
    )
    .unwrap();

    // The old process's SessionEnd now fires, pinned to its own (now stale) pid.
    unregister(db.conn(), "m1/claude_code/reused", Some(old_child.pid())).unwrap();
    assert_ne!(
        resolve(db.conn(), "m1/claude_code/reused").unwrap(),
        Target::NotLive,
        "unregistering with a stale pid must not remove the newer registration"
    );

    // The new process's own SessionEnd, pinned to the stored pid, proceeds.
    unregister(db.conn(), "m1/claude_code/reused", Some(new_child.pid())).unwrap();
    assert_eq!(
        resolve(db.conn(), "m1/claude_code/reused").unwrap(),
        Target::NotLive
    );
}

#[test]
fn dead_pid_is_dropped_and_reports_not_live() {
    let db = Db::open_in_memory().unwrap();
    // A pid that cannot exist.
    register(
        db.conn(),
        "m1/claude_code/sess2",
        2_147_483_647,
        Some("/dev/ttys000"),
        None,
    )
    .unwrap();
    let t = resolve(db.conn(), "m1/claude_code/sess2").unwrap();
    assert_eq!(
        t,
        Target::NotLive,
        "a dead pid must resolve to NotLive, not Unreachable"
    );
    // And the stale binding must have been cleaned up.
    assert_eq!(
        resolve(db.conn(), "m1/claude_code/sess2").unwrap(),
        Target::NotLive
    );
}

#[test]
fn find_session_process_walks_up_to_live_ancestor() {
    // Walk from a child with a deterministic comm (`sleep`) rather than from
    // the test process: a cargo-spawned test binary's comm varies by runner,
    // so it cannot be matched portably.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let found = tp_reach::resolve::find_session_process(child.id() as i32, "sleep");
    assert!(
        found.is_some(),
        "walking up from a `sleep` child must find it by its comm"
    );

    // A needle that exists nowhere must terminate cleanly (None), never panic.
    let absent = tp_reach::resolve::find_session_process(child.id() as i32, "no-such-process");
    let _ = absent;

    let _ = child.kill();
    let _ = child.wait();
}

/// `session_of_process` answers "which registered session am I inside" from
/// the registry, keyed by pid, the one identity that cannot fork; an env var
/// can disagree with what was registered.
#[test]
fn session_of_process_finds_this_processs_own_registration() {
    let db = Db::open_in_memory().unwrap();
    let me = std::process::id() as i32;
    register(db.conn(), "m1/claude_code/mine", me, None, None).unwrap();

    let found = tp_reach::session_of_process(db.conn(), me).unwrap();
    assert_eq!(found.as_deref(), Some("m1/claude_code/mine"));
}

/// `fl` is never the agent; it runs as a descendant of it. Resolution walks
/// up the parent chain to the registered ancestor, which is what makes this
/// work for a runtime with no session env var at all.
#[test]
fn session_of_process_walks_up_to_a_registered_ancestor() {
    let db = Db::open_in_memory().unwrap();
    let parent = unsafe { libc::getppid() };
    register(db.conn(), "m1/pi/ancestor", parent, None, None).unwrap();

    // Asked about ourselves; the answer must come from our parent.
    let found = tp_reach::session_of_process(db.conn(), std::process::id() as i32).unwrap();
    assert_eq!(found.as_deref(), Some("m1/pi/ancestor"));
}

/// Nothing registered anywhere up the chain is a clean `None`, not an error
/// and not a guess; the caller then says so instead of inventing an address.
#[test]
fn session_of_process_is_none_when_no_ancestor_is_registered() {
    let db = Db::open_in_memory().unwrap();
    let found = tp_reach::session_of_process(db.conn(), std::process::id() as i32).unwrap();
    assert!(found.is_none());
}

#[test]
fn runtime_of_host_walks_and_matches_the_nearest_ancestor() {
    // A signature naming this process must match at distance zero. The name
    // is read from the executable rather than written down, so a rename of
    // the test binary does not break an assertion about the walk.
    let me = std::env::current_exe().unwrap();
    let comm = me.file_name().unwrap().to_string_lossy();
    // The hash suffix on the test binary's name would pin the assertion to
    // one build.
    let needle = comm.split('-').next().unwrap().to_string();
    let sigs = vec![("faux".to_string(), needle)];
    assert_eq!(
        tp_reach::resolve::runtime_of_host(std::process::id() as i32, &sigs),
        Some("faux".to_string()),
        "the walk must match this very process"
    );

    // Nothing in the chain declares this: None is the answer, not a guess.
    let none = vec![("nope".to_string(), "zzz-no-such-process-zzz".to_string())];
    assert_eq!(
        tp_reach::resolve::runtime_of_host(std::process::id() as i32, &none),
        None
    );

    // An `=`-anchored pattern is an exact comm, so a substring must not match.
    let exact = vec![("nope".to_string(), "=resolve_test".to_string())];
    assert_eq!(
        tp_reach::resolve::runtime_of_host(std::process::id() as i32, &exact),
        None,
        "`=` means exact — the same rule the scan applies"
    );
}
