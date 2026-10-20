//! The database is owner-only, and an existing one gets repaired. The file
//! mode is the only access control for anything that opens `teleport.db`
//! directly; `tp-app` gates only calls that go through the code.

use std::os::unix::fs::PermissionsExt as _;

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_new_database_and_its_directory_are_owner_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("dot-floonet");
    let path = dir.join("teleport.db");
    drop(tp_db::Db::open(&path).unwrap());

    assert_eq!(mode(&path), 0o600, "database must be owner-only");
    assert_eq!(mode(&dir), 0o700, "the directory must be owner-only too");
}

#[test]
fn the_wal_and_shm_are_covered_while_the_connection_is_open() {
    // These hold uncheckpointed pages and are created at the first write.
    // Checked with the `Db` still alive: SQLite deletes both files when the
    // last connection closes, and an assertion after `drop(db)` would find
    // nothing to check and pass unconditionally.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("teleport.db");
    let db = tp_db::Db::open(&path).unwrap();

    for suffix in ["db-wal", "db-shm"] {
        let side = path.with_extension(suffix);
        assert!(
            side.exists(),
            "{suffix} must exist while the connection is open — if this fails the \
             test below it is checking nothing"
        );
        assert_eq!(mode(&side), 0o600, "{suffix} must be owner-only");
    }
    drop(db);
}

#[test]
fn an_existing_world_readable_database_is_repaired_on_open() {
    // A database is never recreated, so a mode applied only at creation would
    // leave every existing install as it was.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("dot-floonet");
    let path = dir.join("teleport.db");
    drop(tp_db::Db::open(&path).unwrap());

    // Put it back the way an older build left it.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(mode(&path), 0o644, "precondition");

    drop(tp_db::Db::open(&path).unwrap());

    assert_eq!(mode(&path), 0o600, "an existing database must be repaired");
    assert_eq!(mode(&dir), 0o700, "and so must the directory");
}

#[test]
fn a_database_that_cannot_take_a_mode_still_opens() {
    // Best-effort by design: refusing to open would trade reading your own
    // history for a hardening step. An unwritable parent fails the way a
    // chmod on a foreign filesystem does.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("teleport.db");
    drop(tp_db::Db::open(&path).unwrap());
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let opened = tp_db::Db::open(&path);
    // Restore before asserting, or the tempdir cannot be cleaned up.
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        opened.is_ok(),
        "a mode that cannot be set must not fail the open"
    );
}

/// The child half of the test below: it only has to touch `teleport_dir`.
/// Ignored so an ordinary run never executes it — the parent re-runs this
/// binary with `--ignored` and HOME removed, because the environment is
/// process-global and cannot be unset under a threaded test harness.
#[test]
#[ignore = "driven by an_unset_home_does_not_put_the_store_at_the_filesystem_root"]
fn teleport_dir_with_no_home() {
    let _ = tp_db::teleport_dir();
}

/// With HOME unset there is no data directory, and `/.teleport` is not a
/// stand-in for one: `restrict` would chmod that directory to 0700, and the
/// index would be created empty as if the history were gone.
#[test]
fn an_unset_home_does_not_put_the_store_at_the_filesystem_root() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "file_modes::teleport_dir_with_no_home",
            "--exact",
            "--ignored",
        ])
        .env_remove("HOME")
        .output()
        .unwrap();

    // Both streams: the child's harness captures the panic and prints it
    // through its own reporting, so the message lands on stdout.
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "teleport_dir() must refuse to answer with HOME unset, and instead it returned one:\n{said}"
    );
    assert!(
        said.contains("HOME"),
        "the refusal must name what is missing, got: {said}"
    );
}

/// The data directory is `~/.teleport` and must stay that way: it holds the
/// index and the private key, nothing outside this process sees the path, and
/// moving it is the one step in a rename that loses data. A test rather than a
/// comment, because a bulk substitution cannot be told to spare it.
#[test]
fn the_data_directory_is_still_dot_teleport() {
    let tmp = tempfile::tempdir().unwrap();
    // The name is spelled out; a literal built the way the code builds it
    // would pass however the code changed.
    let dir = tp_db::teleport_dir();
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(
        name, ".teleport",
        "the data directory moved. Every existing install's index and key are \
         at ~/.teleport; a rename that touches this loses them silently, because \
         the binary will happily create an empty database at the new path."
    );
    drop(tmp);
}
