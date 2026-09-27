//! Draining a mailbox.
//!
//! "Drain" means read and mark read, as one operation. A read without the
//! mark would re-deliver every message forever; a mark without the return
//! would lose them outright, so the pairing has one home.

use anyhow::Result;
use tp_db::reach::Message;
use tp_db::Db;

/// One drain of a session's mailbox.
pub struct Drained {
    /// The session that was drained, echoed back because a caller that let
    /// floonet work out its own identity has no other way to learn which
    /// mailbox it just emptied.
    pub session_id: String,
    pub messages: Vec<Message>,
}

/// Read every unread message for this session and mark them read.
///
/// If the session belongs to a conversation the read covers every id that
/// conversation has answered to: mail addressed before a compaction sits in a
/// mailbox whose id nothing drains any more. Marking happens after the
/// messages are in hand, so a failure part-way leaves them re-deliverable.
pub fn drain(db: &Db, session_id: &str) -> Result<Drained> {
    let messages = tp_reach::inbox(db.conn(), session_id)?;
    for m in &messages {
        tp_reach::mark_read(db.conn(), &m.id)?;
    }
    Ok(Drained {
        session_id: session_id.to_string(),
        messages,
    })
}

/// Delivered-but-unacked messages: the recovery view.
///
/// `read_at` is set the instant `drain` hands a message over, not when the
/// caller finishes acting on it; an agent interrupted mid-batch leaves
/// messages shown but never confirmed. Read-only: checking pending work must
/// never itself count as having done it.
pub fn pending(db: &Db, session_id: &str) -> Result<Vec<Message>> {
    tp_reach::pending(db.conn(), session_id)
}

/// Acked messages for a session since `since_ms`: "what did that say again",
/// read-only, not a work queue.
pub fn history(db: &Db, session_id: &str, since_ms: tp_core::Millis) -> Result<Vec<Message>> {
    tp_reach::history(db.conn(), session_id, since_ms)
}

/// Confirm a message finished being acted on.
///
/// A message that was never delivered has nothing to confirm and is refused
/// with a reason rather than a silent 0-row update: the id is almost certainly
/// mistyped. Acking an already-acked message is a harmless no-op, so a caller
/// unsure whether its own ack landed can ask twice.
///
/// The write carries its own conditions and the answer is read back from the
/// row, so an acker racing another one is handed the confirmation that stands
/// rather than a time it decided on before writing.
pub fn ack(db: &Db, message_id: &str) -> Result<Message> {
    let id = tp_reach::get_by_prefix(db.conn(), message_id)?.id;
    tp_reach::ack(db.conn(), &id)?;
    let msg = tp_reach::get_by_prefix(db.conn(), &id)?;
    if msg.acked_at.is_none() {
        anyhow::bail!(
            "message {} was never delivered to an inbox — there is nothing to ack",
            &msg.id[..8]
        );
    }
    Ok(msg)
}

/// Which session this process belongs to, with the reason when there is none:
/// several candidates (pick one) and no registration at all (check the daemon)
/// call for different advice and must stay apart.
pub fn own_session(db: &Db, pid: i32) -> Result<tp_reach::OwnSession> {
    tp_reach::own_session(db.conn(), pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{send, Kind};

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    #[test]
    fn draining_returns_the_messages_and_leaves_none_behind() {
        let db = db();
        for body in ["one", "two"] {
            send(&db, "m1", "m1/claude_code/me", body, Kind::Ask, None).unwrap();
        }
        let first = drain(&db, "m1/claude_code/me").unwrap();
        assert_eq!(first.messages.len(), 2);
        assert_eq!(first.session_id, "m1/claude_code/me");

        // A second drain is empty: read and mark are one operation.
        let second = drain(&db, "m1/claude_code/me").unwrap();
        assert!(second.messages.is_empty());
    }

    /// A drained message that was never confirmed must stay findable, without
    /// being indistinguishable from one that was handled.
    #[test]
    fn a_drained_but_unacked_message_is_recoverable_via_pending() {
        let db = db();
        send(
            &db,
            "m1",
            "m1/claude_code/me",
            "do the thing",
            Kind::Ask,
            None,
        )
        .unwrap();
        let drained = drain(&db, "m1/claude_code/me").unwrap();
        assert_eq!(drained.messages.len(), 1, "drain must still show it once");

        // The session that drained it never acked. Drain only shows new
        // messages; recovery goes through the pending view.
        assert!(
            drain(&db, "m1/claude_code/me").unwrap().messages.is_empty(),
            "drain itself must stay clean — pending is a deliberate check, not noise in the default path"
        );
        let pending = pending(&db, "m1/claude_code/me").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "do the thing");
    }

    /// Once acked, a message leaves `pending` and appears in `history`:
    /// recoverable, but no longer "still needs doing".
    #[test]
    fn acking_a_pending_message_moves_it_from_pending_to_history() {
        let db = db();
        send(&db, "m1", "m1/claude_code/me", "hello", Kind::Ask, None).unwrap();
        let msg_id = drain(&db, "m1/claude_code/me").unwrap().messages[0]
            .id
            .clone();

        ack(&db, &msg_id).unwrap();

        assert!(pending(&db, "m1/claude_code/me").unwrap().is_empty());
        let hist = history(&db, "m1/claude_code/me", tp_core::Millis::new(0)).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].id, msg_id);
    }

    /// Acking twice is a retry, not a mistake: a caller unsure whether its
    /// first ack landed must be able to ask again.
    #[test]
    fn acking_an_already_acked_message_is_a_harmless_no_op() {
        let db = db();
        send(&db, "m1", "m1/claude_code/me", "hi", Kind::Ask, None).unwrap();
        let msg_id = drain(&db, "m1/claude_code/me").unwrap().messages[0]
            .id
            .clone();

        let first = ack(&db, &msg_id).unwrap();
        let second = ack(&db, &msg_id).unwrap();
        assert_eq!(
            first.acked_at, second.acked_at,
            "a replayed ack must not move the timestamp forward"
        );
    }

    /// A message that was never delivered has nothing to confirm: refused
    /// with a reason, so a mistyped id is caught.
    #[test]
    fn acking_an_unread_message_is_refused() {
        let db = db();
        let sent = send(&db, "m1", "m1/claude_code/me", "unread", Kind::Ask, None).unwrap();
        let err = ack(&db, &sent.message_id).unwrap_err().to_string();
        assert!(err.contains("never delivered"), "{err}");
    }

    #[test]
    fn an_empty_mailbox_is_not_an_error() {
        let db = db();
        assert!(drain(&db, "m1/claude_code/nobody")
            .unwrap()
            .messages
            .is_empty());
    }

    /// Oldest first, across ids: a conversation's drain spans several mailboxes
    /// and the order has to be the conversation's, not any one mailbox's.
    #[test]
    fn messages_arrive_oldest_first() {
        let db = db();
        for body in ["first", "second", "third"] {
            send(&db, "m1", "m1/claude_code/me", body, Kind::Ask, None).unwrap();
        }
        let bodies: Vec<_> = drain(&db, "m1/claude_code/me")
            .unwrap()
            .messages
            .iter()
            .map(|m| m.body.clone())
            .collect();
        assert_eq!(bodies, ["first", "second", "third"]);
    }
}
