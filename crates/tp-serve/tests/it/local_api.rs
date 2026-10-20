//! The local adapter, end to end over a real socket: the two things most
//! likely to be wrong are the socket's mode and the framing, and calling
//! `dispatch` directly would test neither.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use tp_db::Db;
use tp_net::Identity;

/// One private directory for every socket this file binds, for the run.
///
/// Not `/tmp`: `bind` relies on the containing directory being unreachable to
/// other accounts between creating the socket and chmod'ing it, which is true
/// of a `tempfile` directory (0700) and false of `/tmp`. Leaked on purpose:
/// dropping the `TempDir` removes sockets that `serve` still holds.
fn sock_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"))
        .path()
}

fn sock_path(tag: &str) -> std::path::PathBuf {
    let p = sock_dir().join(format!("{tag}.sock"));
    let _ = std::fs::remove_file(&p);
    // A bind past SUN_LEN fails with a message that names neither the limit
    // nor the path.
    assert!(p.as_os_str().len() < 104, "socket path too long: {p:?}");
    p
}

fn empty_retrieval() -> tp_search::Retrieval {
    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "test-machine",
        vec![],
        vec![],
    )))
}

fn app_with_self() -> tp_app::App {
    let db = Db::open_in_memory().unwrap();
    let id = Identity::generate();
    db.ensure_self_machine(&id.device_id, "TestMac").unwrap();
    tp_app::App::from_parts(db, id, empty_retrieval())
}

/// Start the adapter on `path`, and give the caller a connected client.
async fn start(tag: &str, app: tp_app::App) -> (std::path::PathBuf, UnixStream) {
    let path = sock_path(tag);
    let listener = tp_serve::local::bind(&path).expect("bind");
    tokio::spawn(tp_serve::local::serve(listener, Arc::new(Mutex::new(app))));
    // The listener is bound before `serve` is spawned, so a connect cannot
    // race it: `bind` creates the socket file, not `accept`.
    let client = UnixStream::connect(&path).expect("connect");
    (path, client)
}

fn call(client: &mut UnixStream, line: &str) -> serde_json::Value {
    client.write_all(line.as_bytes()).unwrap();
    client.write_all(b"\n").unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut out = String::new();
    reader.read_line(&mut out).unwrap();
    serde_json::from_str(&out).expect("reply must be one JSON object per line")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_is_not_readable_by_other_accounts() {
    // The only access control this adapter has: `SO_PEERCRED` reports the
    // connecting uid, but the file mode is what keeps another account out.
    use std::os::unix::fs::PermissionsExt as _;
    let (path, _client) = start("mode", app_with_self()).await;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket must be owner-only, got {mode:o}");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capability_answers_and_names_itself_when_it_fails() {
    let (path, mut c) = start("basic", app_with_self()).await;

    let v = call(&mut c, r#"{"capability":"capabilities.list"}"#);
    assert_eq!(v["ok"], true);
    let caps: Vec<&str> = v["result"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(caps.contains(&"live.list"), "got {caps:?}");

    // An unknown verb is an error that names the verb, not a dropped
    // connection: a client has to be able to say "floonet refused X".
    let v = call(&mut c, r#"{"capability":"nope.nope"}"#);
    assert_eq!(v["ok"], false);
    assert_eq!(v["capability"], "nope.nope");
    assert!(v["error"].as_str().unwrap().contains("unknown capability"));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_bad_frame_does_not_take_the_connection_with_it() {
    // The panel holds one connection for its whole run. Dropping it on a
    // malformed line would take every working call after it, so a parse
    // failure is answered like any other error.
    let (path, mut c) = start("frame", app_with_self()).await;

    let v = call(&mut c, "this is not json");
    assert_eq!(v["ok"], false, "a bad frame must be answered, not ignored");

    let v = call(&mut c, r#"{"capability":"daemon.status"}"#);
    assert_eq!(v["ok"], true, "the connection must still work after it");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn absent_daemon_status_is_an_answer_rather_than_an_error() {
    // No `daemon_status` row means the daemon has never started — a fact
    // about the daemon, not a failure to read it.
    let (path, mut c) = start("daemon", app_with_self()).await;
    let v = call(&mut c, r#"{"capability":"daemon.status"}"#);
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["running"], false);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_alias_set_over_the_wire_comes_back_on_the_live_listing() {
    // The one write the panel performs: the alias is stored through `App` and
    // read back through the join that `live.list` does.
    let app = app_with_self();
    let sid = format!("{}/claude_code/conv-x", app.machine_id());
    app.register(
        &sid,
        tp_app::session::Host::Declared(4242),
        Some("/tmp/aliased-project"),
        tp_reach::resolve::Presence::Scan,
        None,
    )
    .unwrap();

    let (path, mut c) = start("alias", app).await;

    let v = call(&mut c, r#"{"capability":"live.list"}"#);
    assert_eq!(v["ok"], true);
    let before = &v["result"]["live"][0];
    assert_eq!(before["cwd"], "/tmp/aliased-project");
    assert!(before["alias"].is_null(), "no alias set yet: {before}");

    let v = call(
        &mut c,
        r#"{"capability":"terminal_alias.set","args":{"cwd":"/tmp/aliased-project","alias":"Aliased"}}"#,
    );
    assert_eq!(v["ok"], true, "set failed: {v}");

    let v = call(&mut c, r#"{"capability":"live.list"}"#);
    assert_eq!(
        v["result"]["live"][0]["alias"], "Aliased",
        "the alias must come back on the listing: {v}"
    );

    // Missing arguments are refused by name rather than silently storing an
    // empty alias.
    let v = call(
        &mut c,
        r#"{"capability":"terminal_alias.set","args":{"cwd":"/tmp/x"}}"#,
    );
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("cwd and alias"));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn message_list_is_bounded_however_much_the_caller_asks_for() {
    // The table only grows, so `limit` is clamped rather than trusted.
    let app = app_with_self();
    let (path, mut c) = start("msg", app).await;
    let v = call(
        &mut c,
        r#"{"capability":"message.list","args":{"limit":100000}}"#,
    );
    assert_eq!(
        v["ok"], true,
        "an absurd limit must be clamped, not refused"
    );
    assert!(v["result"]["messages"].is_array());
    let _ = std::fs::remove_file(&path);
}

/// Binding must not change the mode of a file some other thread is creating:
/// `umask(2)` has no thread scope, and a directory that loses its execute bit
/// is one nothing can write into. The bad window is microseconds wide, so
/// both sides run hot and in parallel rather than once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_does_not_narrow_modes_for_the_rest_of_the_process() {
    use std::os::unix::fs::PermissionsExt as _;
    const ROUNDS: usize = 1000;

    let tmp = tempfile::tempdir().unwrap();
    let dirs = tmp.path().to_path_buf();
    let binds = sock_dir().to_path_buf();

    // Both sides on the blocking pool rather than bare `std::thread`: `bind`
    // hands back a tokio listener, which must be constructed on the runtime.
    //
    // Creator: makes directories at the default mode and reads the mode back
    // rather than writing into it, so the assertion names the actual mode.
    let creator = tokio::task::spawn_blocking(move || {
        let mut bad = Vec::new();
        for i in 0..ROUNDS {
            let d = dirs.join(format!("d{i}"));
            std::fs::create_dir(&d).unwrap();
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            if mode & 0o100 == 0 {
                bad.push(format!("d{i} is {mode:o}"));
            }
        }
        bad
    });

    let binder = tokio::task::spawn_blocking(move || {
        for i in 0..ROUNDS {
            let p = binds.join(format!("race{i}.sock"));
            let _ = std::fs::remove_file(&p);
            let l = tp_serve::local::bind(&p).expect("bind");
            drop(l);
            let _ = std::fs::remove_file(&p);
        }
    });

    binder.await.unwrap();
    let bad = creator.await.unwrap();
    assert!(
        bad.is_empty(),
        "{} of {ROUNDS} directories created during a bind came out without an \
         execute bit, e.g. {:?} — bind is narrowing modes process-wide",
        bad.len(),
        &bad[..bad.len().min(3)]
    );
}
