//! The startup sweep: rows for sessions that were never conversations.
//!
//! The register-time check cannot do this alone: `SessionStart` fires before
//! the transcript has a first line, and "cannot tell" must mean register. A
//! ghost row is not cosmetic, since it carries a pane's pid and tty and a
//! wake to it reports "delivered" to a sender nobody will answer.

use tp_app::App;

/// Descriptor-driven, so the fixture must put transcripts where the runtime's
/// own root points. `claude_code` is the only shipped runtime declaring
/// `non_conversation_types`, and its root follows HOME.
fn write_transcript(home: &std::path::Path, native: &str, kind: &str) {
    let dir = home.join(".claude/projects/-w");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{native}.jsonl")),
        format!("{{\"type\":\"{kind}\",\"uuid\":\"u\",\"sessionId\":\"{native}\"}}\n"),
    )
    .unwrap();
}

#[test]
fn it_removes_internal_sessions_and_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: HOME is process-global; this is the only test in the binary
    // that sets it, and no other thread reads it.
    unsafe { std::env::set_var("HOME", home.path()) };

    let db_path = home.path().join(".teleport/teleport.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let key = home.path().join("key");

    let internal = "00000000-0000-4000-8000-000000000001";
    let real = "00000000-0000-4000-8000-000000000002";
    let absent = "00000000-0000-4000-8000-000000000003";
    write_transcript(home.path(), internal, "queue-operation");
    write_transcript(home.path(), real, "user");
    // `absent` has no transcript.

    {
        let db = tp_db::Db::open(&db_path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        for native in [internal, real, absent] {
            tp_db::reach::upsert_registration(
                db.conn(),
                &format!("m1/claude_code/{native}"),
                4242,
                Some("ttys001"),
                Some("/w"),
                "scan",
                None,
                Some("claude_code"),
                tp_core::Millis::new(1_000),
            )
            .unwrap();
        }
    }

    let app = App::open(&db_path, &key).unwrap();
    let mut judged = std::collections::HashSet::new();
    assert_eq!(
        app.sweep_non_conversations(&mut judged).unwrap(),
        1,
        "exactly the internal session"
    );

    let left: Vec<String> = app
        .live()
        .unwrap()
        .into_iter()
        .map(|r| r.row.session_id)
        .collect();
    assert!(
        !left.iter().any(|s| s.contains(internal)),
        "the internal session must be gone: {left:?}"
    );
    assert!(
        left.iter().any(|s| s.contains(real)),
        "a real conversation must survive: {left:?}"
    );
    // No transcript means no opinion, not "evict": the file may not be
    // written yet.
    assert!(
        left.iter().any(|s| s.contains(absent)),
        "an unreadable transcript must never cost a session its registration: {left:?}"
    );

    // Idempotent: a sweep that kept finding work was not removing what it
    // reported.
    assert_eq!(app.sweep_non_conversations(&mut judged).unwrap(), 0);

    // "Cannot tell" is not a verdict, and must never be cached as one.
    //
    // The sweep runs every cycle rather than once at daemon startup, and the
    // cache is what makes that affordable — a session read once and found to be
    // a real conversation is never read again. But `absent` is the state EVERY
    // session passes through in its first moments: registered, transcript not
    // yet written. Caching that absence would make the sweep permanently blind
    // to the newest rows, which are precisely the ones it exists to remove.
    write_transcript(home.path(), absent, "queue-operation");
    assert_eq!(
        app.sweep_non_conversations(&mut judged).unwrap(),
        1,
        "a transcript that arrived after the first sweep was never re-read: the \
         sweep cached 'cannot tell' as though it were an answer, and is now \
         blind to the rows it exists for"
    );
}
