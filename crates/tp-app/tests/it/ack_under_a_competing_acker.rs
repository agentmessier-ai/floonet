//! Ack is the commit half of a poll/commit pair, and two processes can hold
//! the same message id: one drained it, another was handed it by a person.
//! The confirmation an acker is handed must therefore be the one the database
//! holds, not one it computed from what it read before writing.

use std::time::Duration;

#[test]
fn an_ack_that_lost_the_race_reports_the_stored_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("teleport.db");

    let db = tp_db::Db::open(&db_path).unwrap();
    db.ensure_self_machine("m1", "TestMac").unwrap();
    tp_app::send(
        &db,
        "m1",
        "m1/claude_code/me",
        "do the thing",
        tp_app::Kind::Ask,
        None,
    )
    .unwrap();
    let id = tp_app::drain(&db, "m1/claude_code/me").unwrap().messages[0]
        .id
        .clone();

    // Opened before the lock is taken: a connection that opens afterwards
    // would block in `Db::open` instead of inside the ack under test.
    let racer_db = tp_db::Db::open(&db_path).unwrap();

    // The competing acker holds the write lock, which parks the ack under
    // test between reading the message and writing its confirmation.
    let mut holder = tp_db::Db::open(&db_path).unwrap();
    let txn = holder.begin_immediate().unwrap();

    let racer = {
        let id = id.clone();
        std::thread::spawn(move || tp_app::ack(&racer_db, &id).unwrap())
    };
    std::thread::sleep(Duration::from_millis(100));
    let stored = tp_reach::ack(&txn, &id).unwrap();
    txn.commit().unwrap();
    let reported = racer.join().unwrap();

    assert_eq!(
        reported.acked_at,
        Some(stored),
        "the ack reported a confirmation time no row ever held"
    );
}
