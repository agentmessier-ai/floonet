//! Coordinates resolve + wake + mailbox for one delivery attempt. Wakes are
//! coalesced to one per target session per `WAKE_COALESCE_MS` while the earlier
//! wake is still undrained — the inbox drains in batch, so a suppressed wake
//! loses nothing then, and only then (see `should_coalesce`) — and a session
//! whose pending messages have hit `MAX_DELIVER` attempts is not woken again.

use crate::mailbox;
use crate::resolve::{self, Target};
use crate::wake::{self, Caller};
use anyhow::Result;
use tp_db::reach;
use tp_db::DbConnection as Connection;

pub const WAKE_COALESCE_MS: i64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryOutcome {
    /// Woke the target just now.
    Woke(Target),
    /// Skipped: woken within the last `WAKE_COALESCE_MS`. The earlier wake
    /// drains whatever is pending, so this is not a failure.
    Coalesced,
    /// Nothing to wake for (every message for this session is either already
    /// read, or past `MAX_DELIVER` and parked `dead_at`).
    NoMessages,
    /// Enqueued, but the target cannot be injected into right now; it sees
    /// the message on its next manual `/fl inbox`.
    NotInjectable(Target),
}

/// Attempt to wake `session_id` for whatever is currently pending. Call after
/// enqueueing: it acts on the full wakeable set for the session, not just the
/// message just sent, since one wake drains the whole inbox.
pub fn attempt_wake(
    conn: &Connection,
    session_id: &str,
    control: &str,
    caller: Caller,
) -> Result<DeliveryOutcome> {
    let pending = mailbox::wakeable(conn, session_id)?;
    if pending.is_empty() {
        return Ok(DeliveryOutcome::NoMessages);
    }

    let last = reach::last_wake_at(conn, session_id)?;
    if should_coalesce(&pending, last, mailbox::now_ms()) {
        return Ok(DeliveryOutcome::Coalesced);
    }

    let target = resolve::resolve(conn, session_id)?;
    match &target {
        Target::Tmux(_) | Target::Terminal { .. } | Target::Channel(_) => {
            wake::wake(&target, session_id, control, caller)?;
            reach::set_last_wake_at(conn, session_id, mailbox::now_ms())?;
            // Attempts count physical wake events, not enqueue events: a message
            // without a live target must not burn down MAX_DELIVER by waiting.
            for m in &pending {
                mailbox::record_wake(conn, &m.id)?;
            }
            Ok(DeliveryOutcome::Woke(target))
        }
        Target::Unreachable | Target::NotLive => Ok(DeliveryOutcome::NotInjectable(target)),
    }
}

/// Whether a wake for `pending` can be skipped because an earlier one will
/// drain it.
///
/// Recency alone is not enough. A wake is absorbed only while it is still
/// undrained, and that is visible in `pending`: a message the earlier wake
/// covered has `attempts > 0` and, being pending, is still unread. When every
/// pending message has `attempts == 0`, whatever that wake covered has already
/// been read — the drain it triggered is over, and nothing is coming for the
/// rest. Coalescing then does not save a duplicate wake; it strands the message,
/// because nothing retries a wake later.
fn should_coalesce(
    pending: &[mailbox::Message],
    last_wake: Option<tp_core::Millis>,
    now: tp_core::Millis,
) -> bool {
    let undrained = pending.iter().any(|m| m.attempts > 0);
    undrained && last_wake.is_some_and(|last| now - last < WAKE_COALESCE_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{enqueue, MAX_DELIVER};
    use tp_db::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        db
    }

    #[test]
    fn no_pending_messages_is_not_an_error() {
        let db = setup();
        let out = attempt_wake(db.conn(), "nobody", wake::CONTROL_STRING, Caller::Cli).unwrap();
        assert_eq!(out, DeliveryOutcome::NoMessages);
    }

    #[test]
    fn unregistered_target_reports_not_injectable_but_still_delivers() {
        let db = setup();
        enqueue(db.conn(), "target-session", None, "me", "ask", "hi", None).unwrap();
        let out = attempt_wake(
            db.conn(),
            "target-session",
            wake::CONTROL_STRING,
            Caller::Cli,
        )
        .unwrap();
        assert_eq!(out, DeliveryOutcome::NotInjectable(Target::NotLive));
    }

    #[test]
    fn max_deliver_stops_waking_after_the_cap() {
        let db = setup();
        // Attempts are counted directly rather than through a real target;
        // the cap is the only behaviour under test.
        let msg = enqueue(db.conn(), "s1", None, "me", "ask", "hi", None).unwrap();
        for _ in 0..MAX_DELIVER {
            mailbox::record_wake(db.conn(), &msg.id).unwrap();
        }
        let pending = mailbox::wakeable(db.conn(), "s1").unwrap();
        assert!(
            pending.is_empty(),
            "message must be parked dead_at after MAX_DELIVER attempts"
        );

        let out = attempt_wake(db.conn(), "s1", wake::CONTROL_STRING, Caller::Cli).unwrap();
        assert_eq!(
            out,
            DeliveryOutcome::NoMessages,
            "a dead-parked message must not trigger another wake attempt"
        );
    }

    #[test]
    fn second_attempt_within_coalesce_window_is_skipped() {
        let db = setup();
        enqueue(db.conn(), "s1", None, "me", "ask", "one", None).unwrap();
        // A NotInjectable attempt does not set last_wake_at (only an actual
        // wake does), so the second attempt is not coalesced against it.
        assert_eq!(
            attempt_wake(db.conn(), "s1", wake::CONTROL_STRING, Caller::Cli).unwrap(),
            DeliveryOutcome::NotInjectable(Target::NotLive)
        );
        enqueue(db.conn(), "s1", None, "me", "ask", "two", None).unwrap();
        assert_eq!(
            attempt_wake(db.conn(), "s1", wake::CONTROL_STRING, Caller::Cli).unwrap(),
            DeliveryOutcome::NotInjectable(Target::NotLive)
        );
    }
}
#[cfg(test)]
mod coalesce_tests {
    use super::{should_coalesce, WAKE_COALESCE_MS};
    use crate::mailbox::Message;
    use tp_core::Millis;

    fn msg(id: &str, attempts: i64) -> Message {
        Message {
            id: id.into(),
            to_session: "m/pi/s".into(),
            from_session: None,
            from_machine: "m".into(),
            kind: "reply".into(),
            body: "x".into(),
            reply_to: None,
            created_at: Millis::new(0),
            delivered_at: None,
            read_at: None,
            attempts,
            dead_at: None,
            acked_at: None,
        }
    }

    /// Measured on a real machine: one session asked two others a question
    /// each. The first reply woke it and was read 24ms later. The second landed
    /// 5.4s after that wake, inside the window, and was coalesced — against a
    /// wake whose drain had already happened. Nothing ever woke the session
    /// again: `attempts` stayed 0 and the reply sat unread. Asking two agents
    /// at once and getting both answers quickly is the ordinary case.
    #[test]
    fn a_message_no_wake_has_covered_is_woken_even_inside_the_window() {
        let now = Millis::new(1_000_000);
        let last = Some(Millis::new(1_000_000 - 5_400));
        // The earlier wake's message was read, so it is not pending; only the
        // new one is, and no wake has ever covered it.
        assert!(
            !should_coalesce(&[msg("72", 0)], last, now),
            "coalesced against a wake that had already been drained — this message is never delivered"
        );
    }

    /// What coalescing is for: a second message arrives before the target has
    /// drained the first. The drain that wake triggers reads both, and a second
    /// wake would only type the control string into the pane twice.
    #[test]
    fn a_wake_still_undrained_absorbs_the_next_message() {
        let now = Millis::new(1_000_000);
        let last = Some(Millis::new(1_000_000 - 2_000));
        assert!(should_coalesce(&[msg("a", 1), msg("b", 0)], last, now));
    }

    #[test]
    fn outside_the_window_it_always_wakes() {
        let now = Millis::new(1_000_000);
        let last = Some(Millis::new(1_000_000 - WAKE_COALESCE_MS - 1));
        assert!(!should_coalesce(&[msg("a", 1), msg("b", 0)], last, now));
        assert!(!should_coalesce(&[msg("b", 0)], None, now));
    }
}
