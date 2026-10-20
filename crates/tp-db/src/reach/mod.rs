//! Data access for the tables reach owns: `live_session`, `conversation` and
//! `message`. SQL lives next to the schema it queries; callers get typed rows,
//! never a `Connection` method call, so a column change is visible in one
//! crate.
//!
//! Not an ORM or query builder: every statement is a static literal (only
//! `prune_scan_rows` expands a placeholder list), and rusqlite keeps the
//! callers synchronous. Policy stays with the caller: functions take `now`
//! rather than reading the clock, and return counts rather than deciding what
//! a count means.
//!
//! Split by decision rather than by table: presence, addressing and the
//! mailbox change for different reasons and at different times. The
//! submodules are re-exported flat, so every caller keeps the
//! `tp_db::reach::…` path it already uses.

mod address;
mod mailbox;
mod presence;

pub use address::*;
pub use mailbox::*;
pub use presence::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    fn live(db: &Db, session_id: &str, pid: i32) {
        db.conn()
            .execute(
                "INSERT INTO live_session(session_id, pid, source, registered_at, last_seen_at, presence)
                 VALUES (?1, ?2, 'scan', 0, 0, 'scan')",
                rusqlite::params![session_id, pid],
            )
            .unwrap();
    }

    /// A bare native id resolves to the runtime it is registered under, rather
    /// than a default guessed from its shape.
    #[test]
    fn a_bare_native_id_resolves_to_the_runtime_it_is_registered_under() {
        let db = db();
        live(&db, "m1/codex/abc-123", 1);
        assert_eq!(
            runtimes_for_native(db.conn(), "abc-123").unwrap(),
            ["codex"]
        );
    }

    /// Two answers are not an answer. Reported as two so the caller decides,
    /// rather than picked for it.
    #[test]
    fn the_same_native_id_under_two_runtimes_is_reported_as_both() {
        let db = db();
        live(&db, "m1/codex/dup", 1);
        live(&db, "m1/pi/dup", 2);
        assert_eq!(
            runtimes_for_native(db.conn(), "dup").unwrap(),
            ["codex", "pi"]
        );
    }

    /// The LIKE pattern also matches a native id that merely ends in the
    /// query; the segment itself has to match.
    #[test]
    fn a_native_id_that_is_only_a_suffix_does_not_match() {
        let db = db();
        live(&db, "m1/codex/abc-123", 1);
        assert!(runtimes_for_native(db.conn(), "123").unwrap().is_empty());
    }

    /// A native id may itself contain `/` (`SessionId` splits on the first two
    /// separators only), so the runtime segment is the second one, not the
    /// whole of what follows it.
    #[test]
    fn a_native_id_containing_a_slash_resolves_to_its_runtime() {
        let db = db();
        live(&db, "m1/codex/nested/path/id", 1);
        assert_eq!(
            runtimes_for_native(db.conn(), "nested/path/id").unwrap(),
            ["codex"]
        );
    }

    /// A prefix is matched literally: `%` and `_` are LIKE metacharacters, and
    /// a caller's prefix that contains one must not widen the match.
    #[test]
    fn a_prefix_is_literal_not_a_like_pattern() {
        let db = db();
        let msg = |id: &str| {
            insert_message(
                db.conn(),
                id,
                "m1/codex/s1",
                None,
                "m1",
                "note",
                "hi",
                None,
                tp_core::Millis::new(1),
            )
            .unwrap()
        };
        msg("abc123");

        assert_eq!(by_prefix(db.conn(), "abc").unwrap().len(), 1);
        assert!(
            by_prefix(db.conn(), "%").unwrap().is_empty(),
            "a prefix of '%' must match no id, not every id"
        );
        assert!(
            by_prefix(db.conn(), "a_c").unwrap().is_empty(),
            "'_' must be a literal underscore, not any-character"
        );
    }

    /// A row that cannot be read is not an absent row. Dropping it would hand
    /// the caller a short list of twins as if it were the whole pane.
    #[test]
    fn a_pane_whose_twin_row_cannot_be_read_is_an_error() {
        let db = db();
        let conn = db.conn();
        let conv = |id_sql: &str| {
            conn.execute(
                &format!(
                    "INSERT INTO conversation(id, machine_id, runtime_id, pid, pid_start, cwd,
                                              created_at, last_seen_at)
                     VALUES ({id_sql}, 'm1', 'codex', 7, NULL, '/tmp', 0, 0)"
                ),
                [],
            )
            .unwrap();
        };
        conv("'c-good'");
        // Text that is not UTF-8: storable, unreadable as a Rust `String`.
        conv("CAST(X'FF' AS TEXT)");
        conn.execute(
            "INSERT INTO conversation_member(session_id, conversation_id, joined_at)
             VALUES ('m1/codex/s1', 'c-good', 0)",
            [],
        )
        .unwrap();

        assert!(
            conversations_of_pane(conn, "m1/codex/s1").is_err(),
            "an unreadable twin must be reported, not silently omitted"
        );
    }

    #[test]
    fn an_unregistered_native_id_resolves_to_nothing() {
        let db = db();
        assert!(runtimes_for_native(db.conn(), "never-seen")
            .unwrap()
            .is_empty());
    }

    /// Every statement in this module must still be valid against the schema
    /// the migrations produce, so a renamed or dropped column fails here rather
    /// than at a call site. Not a substitute for the behavioural tests: a query
    /// can compile and still answer the wrong question.
    #[test]
    fn every_statement_compiles_against_the_migrated_schema() {
        let db = db();
        let conn = db.conn();
        let pids = [1, 2, 3];
        scan_rows_for_pid(conn, 1).unwrap();
        rows_for_pid(conn, 1).unwrap();
        list_live(conn).unwrap();
        addressability(conn, "s").unwrap();
        target_row(conn, "s").unwrap();
        upsert_registration(
            conn,
            "s",
            1,
            None,
            None,
            "scan",
            None,
            None,
            tp_core::Millis::new(0),
        )
        .unwrap();
        insert_scanned(conn, "s2", 2, None, None, tp_core::Millis::new(0)).unwrap();
        touch_location(conn, "s", None, None, tp_core::Millis::new(1)).unwrap();
        delete_session_pinned(conn, "s2", 2).unwrap();
        prune_scan_rows(conn, &pids).unwrap();
        prune_scan_rows(conn, &[]).unwrap();
        touch_heartbeat(conn, "s", tp_core::Millis::new(1)).unwrap();
        mark_stale(conn, tp_core::Millis::new(1), tp_core::Millis::new(0)).unwrap();
        evict_stale(conn, tp_core::Millis::new(0)).unwrap();
        last_wake_at(conn, "s").unwrap();
        set_last_wake_at(conn, "s", tp_core::Millis::new(1)).unwrap();
        delete_session(conn, "s").unwrap();

        insert_message(
            conn,
            "m",
            "s",
            None,
            "m1",
            "ask",
            "hi",
            None,
            tp_core::Millis::new(0),
        )
        .unwrap();
        unread(conn, "s").unwrap();
        wakeable(conn, "s").unwrap();
        by_prefix(conn, "m").unwrap();
        attempts_of(conn, "m").unwrap();
        bump_attempt(conn, "m", tp_core::Millis::new(0), 5).unwrap();
        mark_read(conn, "m", tp_core::Millis::new(0)).unwrap();
    }

    /// The three states must stay distinguishable; collapsed, an undeliverable
    /// send is reported as delivered.
    #[test]
    fn addressability_separates_registered_dormant_and_unknown() {
        let db = db();
        let conn = db.conn();
        db.ensure_runtime("claude_code", "/root").unwrap();
        upsert_registration(
            conn,
            "m1/claude_code/live",
            1,
            None,
            None,
            "scan",
            None,
            None,
            tp_core::Millis::new(0),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session(id, machine_id, runtime_id, native_id, turn_count)
             VALUES ('m1/claude_code/old', 'm1', 'claude_code', 'old', 0)",
            [],
        )
        .unwrap();

        assert_eq!(
            addressability(conn, "m1/claude_code/live").unwrap(),
            StoredAddressability::Registered
        );
        assert_eq!(
            addressability(conn, "m1/claude_code/old").unwrap(),
            StoredAddressability::Dormant,
            "an indexed session with no live row is dormant, not unknown — its id most likely rotated"
        );
        assert_eq!(
            addressability(conn, "m1/claude_code/never").unwrap(),
            StoredAddressability::Unknown
        );
    }

    /// The rotation this layer exists for: a compaction registers a new
    /// session id from the same process, and the address must not change.
    #[test]
    fn a_compaction_rejoins_the_same_conversation() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/Users/me/dev/proj");

        let a = join_conversation(
            conn,
            "m1/claude_code/aaa",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-1",
        )
        .unwrap();
        // Same pid, same cwd, moments later: the only signal a compaction
        // gives, since transcripts carry no link between an id and its
        // successor.
        let b = join_conversation(
            conn,
            "m1/claude_code/bbb",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(2_000),
            "m1/claude_code/conv-2",
        )
        .unwrap();
        assert_eq!(a, b, "a rotation must keep the conversation address");
        let members: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversation_member WHERE conversation_id = ?1",
                [&a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(members, 2);

        // Re-running the hook must not re-parent an already-bound session.
        let again = join_conversation(
            conn,
            "m1/claude_code/aaa",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(3_000),
            "m1/claude_code/conv-3",
        )
        .unwrap();
        assert_eq!(again, a);
    }

    /// A return address is written when a message is sent and used when it is
    /// answered, and everything can rotate in between.
    #[test]
    fn a_stored_return_address_still_resolves_after_the_sender_rotates() {
        let db = db();
        let conn = db.conn();
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 5,
            pid_start: None,
            cwd: Some("/p"),
        };
        let conv = join_conversation(
            conn,
            "m1/claude_code/old",
            key,
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-r",
        )
        .unwrap();
        join_conversation(
            conn,
            "m1/claude_code/new",
            key,
            tp_core::Millis::new(2_000),
            "unused",
        )
        .unwrap();
        upsert_registration(
            conn,
            "m1/claude_code/new",
            5,
            None,
            Some("/p"),
            "scan",
            None,
            Some("claude_code"),
            tp_core::Millis::new(2_100),
        )
        .unwrap();

        // Stamped with the conversation address.
        assert_eq!(
            conversation_current_session(conn, &conv)
                .unwrap()
                .as_deref(),
            Some("m1/claude_code/new")
        );
        // Stamped with a segment id that has since compacted away: the
        // membership is what forwards it.
        assert_eq!(
            conversation_of(conn, "m1/claude_code/old")
                .unwrap()
                .as_deref(),
            Some(conv.as_str())
        );
    }

    /// The scan may join a conversation but must never create one: its session
    /// id is a guess, good enough to publish as an address but not to seed an
    /// identity other sessions will be told to write to.
    #[test]
    fn a_scanned_row_joins_an_existing_conversation_and_never_mints_one() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/p");
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 42,
            pid_start: None,
            cwd,
        };

        // Nothing registered yet: the scan must leave no trace.
        assert_eq!(
            join_existing_conversation(
                conn,
                "m1/claude_code/guess",
                key,
                tp_core::Millis::new(1_000)
            )
            .unwrap(),
            None
        );
        let convs: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversation", [], |r| r.get(0))
            .unwrap();
        assert_eq!(convs, 0, "a guessed id must not create a correspondent");

        // Once the hook has registered a real session on that process, the
        // scan's id joins it; otherwise the id is wakeable but drained by
        // nobody.
        let conv = join_conversation(
            conn,
            "m1/claude_code/real",
            key,
            tp_core::Millis::new(2_000),
            "m1/claude_code/conv-1",
        )
        .unwrap();
        assert_eq!(
            join_existing_conversation(
                conn,
                "m1/claude_code/guess",
                key,
                tp_core::Millis::new(2_100)
            )
            .unwrap()
            .as_deref(),
            Some(conv.as_str())
        );
        let members: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversation_member WHERE conversation_id = ?1",
                [&conv],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(members, 2);

        // Mail addressed to the resurrected id is now collected.
        insert_message(
            conn,
            "m",
            "m1/claude_code/guess",
            None,
            "m1",
            "ask",
            "to the old address",
            None,
            tp_core::Millis::new(2_200),
        )
        .unwrap();
        assert_eq!(unread_for_conversation(conn, &conv).unwrap().len(), 1);
    }

    /// A conversation is only refreshed when it is joined. Without the scan
    /// refreshing it, a process running for hours without compacting would fall
    /// out of the grace window and stop being joinable.
    #[test]
    fn scanning_keeps_a_quiet_conversation_inside_the_grace_window() {
        let db = db();
        let conn = db.conn();
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 7,
            pid_start: None,
            cwd: Some("/p"),
        };
        let conv = join_conversation(
            conn,
            "m1/claude_code/a",
            key,
            tp_core::Millis::new(0),
            "m1/claude_code/conv-q",
        )
        .unwrap();

        // Several scan cycles, each well inside the window relative to the last.
        let mut t = 0;
        for _ in 0..5 {
            t += CONVERSATION_JOIN_GRACE_MS / 2;
            assert!(join_existing_conversation(
                conn,
                "m1/claude_code/a",
                key,
                tp_core::Millis::new(t)
            )
            .unwrap()
            .is_some());
        }
        // A new id far past the original registration is joinable only
        // because the scan kept the conversation warm.
        t += CONVERSATION_JOIN_GRACE_MS / 2;
        assert_eq!(
            join_existing_conversation(conn, "m1/claude_code/b", key, tp_core::Millis::new(t))
                .unwrap()
                .as_deref(),
            Some(conv.as_str())
        );
    }

    /// The point of `pid_start`: recognition stops depending on anything being
    /// kept warm. A compaction long past the grace window, with nothing having
    /// touched the row, still rejoins because the process is the same
    /// incarnation.
    #[test]
    fn an_incarnation_rejoins_no_matter_how_long_the_gap() {
        let db = db();
        let conn = db.conn();
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 100,
            pid_start: Some("Sat Aug 15 11:21:25 2026"),
            cwd: Some("/p"),
        };
        let a = join_conversation(
            conn,
            "m1/claude_code/a",
            key,
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-1",
        )
        .unwrap();
        let much_later = 1_000 + CONVERSATION_JOIN_GRACE_MS * 100;
        let b = join_conversation(
            conn,
            "m1/claude_code/b",
            key,
            tp_core::Millis::new(much_later),
            "m1/claude_code/conv-2",
        )
        .unwrap();
        assert_eq!(a, b, "an address must not expire while its process runs");
    }

    /// The reuse `pid_start` defeats: same pid, cwd and runtime one second
    /// later, but a different process, and the start time says so where a time
    /// window would have said "close enough".
    #[test]
    fn a_reused_pid_is_a_different_correspondent_even_one_second_later() {
        let db = db();
        let conn = db.conn();
        let first = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 100,
            pid_start: Some("Sat Aug 15 11:21:25 2026"),
            cwd: Some("/p"),
        };
        let reused = ConversationKey {
            pid_start: Some("Sat Aug 15 11:21:26 2026"),
            ..first
        };
        let a = join_conversation(
            conn,
            "m1/claude_code/a",
            first,
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-1",
        )
        .unwrap();
        let b = join_conversation(
            conn,
            "m1/claude_code/b",
            reused,
            tp_core::Millis::new(1_001),
            "m1/claude_code/conv-2",
        )
        .unwrap();
        assert_ne!(
            a, b,
            "delivering one agent's mail to another is worse than minting an address"
        );
    }

    /// A row with no start time falls back to the window, which is weaker but
    /// never wrong-by-merge.
    #[test]
    fn an_unknown_start_time_falls_back_to_the_window() {
        let db = db();
        let conn = db.conn();
        let legacy = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 100,
            pid_start: None,
            cwd: Some("/p"),
        };
        let a = join_conversation(
            conn,
            "m1/claude_code/a",
            legacy,
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-1",
        )
        .unwrap();
        assert_eq!(
            join_conversation(
                conn,
                "m1/claude_code/b",
                legacy,
                tp_core::Millis::new(1_100),
                "unused"
            )
            .unwrap(),
            a,
            "inside the window, a legacy row still recognises its process"
        );
        assert_ne!(
            join_conversation(
                conn,
                "m1/claude_code/c",
                legacy,
                // Past the window measured from the last join, not the first:
                // the fallback window slides with activity.
                tp_core::Millis::new(1_100 + CONVERSATION_JOIN_GRACE_MS + 1),
                "m1/claude_code/conv-3"
            )
            .unwrap(),
            a,
            "past it, the old behaviour still applies"
        );
    }

    /// Each part of the join key has to hold on its own: a false merge delivers
    /// one agent's mail to another, worse than minting one address too many.
    #[test]
    fn unrelated_sessions_never_merge() {
        let db = db();
        let conn = db.conn();
        let base = join_conversation(
            conn,
            "m1/claude_code/a",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd: Some("/p"),
            },
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-a",
        )
        .unwrap();

        // Different cwd: two concurrent sessions can share nothing but a pid
        // namespace, and a directory is what tells them apart.
        let other_dir = join_conversation(
            conn,
            "m1/claude_code/b",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd: Some("/other"),
            },
            tp_core::Millis::new(1_100),
            "m1/claude_code/conv-b",
        )
        .unwrap();
        assert_ne!(base, other_dir);

        // Different runtime on the same pid — a multiplexed host.
        let other_rt = join_conversation(
            conn,
            "m1/pi/c",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "pi",
                pid: 100,
                pid_start: None,
                cwd: Some("/p"),
            },
            tp_core::Millis::new(1_200),
            "m1/pi/conv-c",
        )
        .unwrap();
        assert_ne!(base, other_rt);

        // Same everything, but long after: pid reuse, not a rotation.
        let reused = join_conversation(
            conn,
            "m1/claude_code/d",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd: Some("/p"),
            },
            tp_core::Millis::new(1_000 + CONVERSATION_JOIN_GRACE_MS + 1),
            "m1/claude_code/conv-d",
        )
        .unwrap();
        assert_ne!(
            base, reused,
            "past the grace window a shared pid means nothing"
        );
    }

    /// Mail sent to an id that has since rotated away is still collectable.
    #[test]
    fn draining_a_conversation_collects_mail_sent_to_a_retired_id() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/p");
        let conv = join_conversation(
            conn,
            "m1/claude_code/old",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-x",
        )
        .unwrap();
        insert_message(
            conn,
            "msg-old",
            "m1/claude_code/old",
            None,
            "m1",
            "ask",
            "sent before the compaction",
            None,
            tp_core::Millis::new(1_500),
        )
        .unwrap();

        join_conversation(
            conn,
            "m1/claude_code/new",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(2_000),
            "m1/claude_code/conv-y",
        )
        .unwrap();
        insert_message(
            conn,
            "msg-new",
            "m1/claude_code/new",
            None,
            "m1",
            "ask",
            "sent after",
            None,
            tp_core::Millis::new(2_500),
        )
        .unwrap();

        let drained = unread_for_conversation(conn, &conv).unwrap();
        assert_eq!(
            drained.len(),
            2,
            "the retired id's mailbox must be drained too"
        );
        assert_eq!(drained[0].id, "msg-old", "oldest first, across ids");

        // The per-session read still sees only its own: the conversation union
        // is an addition, not a replacement.
        assert_eq!(unread(conn, "m1/claude_code/new").unwrap().len(), 1);
    }

    /// Addressing resolves to the segment that is actually registered, so a
    /// wake lands on the live one rather than a retired sibling.
    #[test]
    fn a_conversation_resolves_to_its_registered_segment() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/p");
        let conv = join_conversation(
            conn,
            "m1/claude_code/old",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-z",
        )
        .unwrap();
        join_conversation(
            conn,
            "m1/claude_code/new",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            tp_core::Millis::new(2_000),
            "unused",
        )
        .unwrap();

        // Only the older one is registered: newest-member order must not win
        // over being live.
        upsert_registration(
            conn,
            "m1/claude_code/old",
            7,
            None,
            cwd,
            "scan",
            None,
            Some("claude_code"),
            tp_core::Millis::new(3_000),
        )
        .unwrap();
        assert_eq!(
            conversation_current_session(conn, &conv)
                .unwrap()
                .as_deref(),
            Some("m1/claude_code/old")
        );

        // With nothing registered, fall back to the newest member rather than
        // refusing: the message still lands where the next drain finds it.
        delete_session(conn, "m1/claude_code/old").unwrap();
        assert_eq!(
            conversation_current_session(conn, &conv)
                .unwrap()
                .as_deref(),
            Some("m1/claude_code/new")
        );
    }

    #[test]
    fn prune_spares_declared_rows_and_takes_unfound_scan_rows() {
        let db = db();
        let conn = db.conn();
        upsert_registration(
            conn,
            "hosted",
            10,
            None,
            None,
            "declared",
            None,
            None,
            tp_core::Millis::new(0),
        )
        .unwrap();
        insert_scanned(conn, "scanned", 11, None, None, tp_core::Millis::new(0)).unwrap();

        prune_scan_rows(conn, &[10]).unwrap();
        assert!(target_row(conn, "hosted").unwrap().is_some());
        assert!(
            target_row(conn, "scanned").unwrap().is_none(),
            "a scan row whose pid the scan no longer finds must be pruned"
        );

        // Even an empty scan leaves declared rows.
        prune_scan_rows(conn, &[]).unwrap();
        assert!(target_row(conn, "hosted").unwrap().is_some());
    }

    #[test]
    fn a_heartbeat_clears_a_stale_mark_and_reports_whether_a_row_existed() {
        let db = db();
        let conn = db.conn();
        upsert_registration(
            conn,
            "s",
            1,
            None,
            None,
            "declared",
            None,
            None,
            tp_core::Millis::new(0),
        )
        .unwrap();
        assert_eq!(
            mark_stale(conn, tp_core::Millis::new(500), tp_core::Millis::new(400)).unwrap(),
            1
        );
        assert!(target_row(conn, "s").unwrap().unwrap().stale_at.is_some());

        assert_eq!(
            touch_heartbeat(conn, "s", tp_core::Millis::new(600)).unwrap(),
            1
        );
        assert!(target_row(conn, "s").unwrap().unwrap().stale_at.is_none());
        assert_eq!(
            touch_heartbeat(conn, "gone", tp_core::Millis::new(600)).unwrap(),
            0,
            "beating into an evicted session must report that it is gone"
        );
    }
}
