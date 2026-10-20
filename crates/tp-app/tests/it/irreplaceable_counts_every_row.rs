//! The irreplaceable count decides whether losing the database file is an
//! inconvenience or a loss, so a row the reader cannot decode must stop the
//! count rather than shrink it: a smaller number reads as better news.

use tp_app::App;

/// Drop `STRICT` from `session` so a row can hold a value the reader's types
/// reject — the shape schema drift or a damaged page leaves behind.
fn relax_session_column_types(path: &std::path::Path) {
    let db = tp_db::Db::open(path).unwrap();
    db.conn()
        .execute_batch(
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_schema SET sql = replace(sql, ') STRICT', ')')
              WHERE type = 'table' AND name = 'session';
             PRAGMA writable_schema = RESET;",
        )
        .unwrap();
}

fn insert_session(db: &tp_db::Db, id: &str, native: &str, turn_count: &str) {
    db.conn()
        .execute_batch(&format!(
            "INSERT INTO session (id, machine_id, runtime_id, native_id, turn_count)
             VALUES ('{id}', 'm1', 'claude_code', '{native}', {turn_count})"
        ))
        .unwrap();
}

#[test]
fn a_session_row_that_cannot_be_read_fails_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("teleport.db");
    {
        let db = tp_db::Db::open(&db_path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        insert_session(&db, "good", "n1", "3");
    }
    relax_session_column_types(&db_path);
    {
        let db = tp_db::Db::open(&db_path).unwrap();
        insert_session(&db, "bad", "n2", "'not a number'");
    }

    let app = App::open(&db_path, &dir.path().join("key")).unwrap();
    let counted = app.irreplaceable();
    assert!(
        counted.is_err(),
        "a row that could not be read was dropped and the rest handed back as the whole answer: {counted:?}"
    );
}
