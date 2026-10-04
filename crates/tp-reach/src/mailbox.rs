//! Mailbox: messages live in the DB, never typed into a peer pane. The only
//! thing that crosses into a pane is a fixed control string; the target reads
//! the body from the DB via `/fl inbox`.
//!
//! A session that never drains its inbox must not be woken forever: past
//! `MAX_DELIVER` attempts the row is marked `dead_at`, stays readable, and
//! never triggers another wake. The SQL lives in `tp_db::reach`, next to the
//! migrations; what stays here is policy: the cap, the clock, prefix lookup.

use anyhow::Result;
use tp_db::reach;
use tp_db::DbConnection as Connection;

pub use tp_db::reach::Message;

/// Max wake attempts before a message is parked as dead.
pub const MAX_DELIVER: i64 = 5;

/// Enqueue a message into a target's mailbox.
pub fn enqueue(
    conn: &Connection,
    to_session: &str,
    from_session: Option<&str>,
    from_machine: &str,
    kind: &str,
    body: &str,
    reply_to: Option<&str>,
) -> Result<Message> {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = now_ms();
    reach::insert_message(
        conn,
        &id,
        to_session,
        from_session,
        from_machine,
        kind,
        body,
        reply_to,
        created_at,
    )?;
    Ok(Message {
        id,
        to_session: to_session.to_string(),
        from_session: from_session.map(|s| s.to_string()),
        from_machine: from_machine.to_string(),
        kind: kind.to_string(),
        body: body.to_string(),
        reply_to: reply_to.map(|s| s.to_string()),
        created_at,
        delivered_at: None,
        read_at: None,
        attempts: 0,
        dead_at: None,
        acked_at: None,
    })
}

/// Read across every conversation this pane owns, not just the one the current
/// session belongs to: a pane can own several (see `conversations_of_pane`),
/// and mail addressed to one twin would otherwise be invisible to a session
/// sitting in the other. Sorted by the message clock so a drain spanning
/// several mailboxes still hands messages over oldest-first.
fn across_pane(
    conn: &Connection,
    session_id: &str,
    per_conversation: impl Fn(&Connection, &str) -> Result<Vec<Message>>,
    fallback: impl FnOnce(&Connection, &str) -> Result<Vec<Message>>,
) -> Result<Vec<Message>> {
    let convs = reach::conversations_of_pane(conn, session_id)?;
    if convs.is_empty() {
        // No conversation at all (a runtime that never registers): its own mailbox.
        return fallback(conn, session_id);
    }
    let mut out = Vec::new();
    for c in &convs {
        out.extend(per_conversation(conn, c)?);
    }
    out.sort_by_key(|m| m.created_at);
    Ok(out)
}

/// Unread messages for a session (the `/fl inbox` drain). If the session
/// belongs to a conversation, the drain covers every id that conversation has
/// answered to: mail addressed before a compaction sits in a mailbox whose id
/// nothing drains any more, and collecting it is the point of a conversation
/// address. A session with no conversation reads exactly its own mailbox.
pub fn inbox(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    across_pane(
        conn,
        session_id,
        reach::unread_for_conversation,
        reach::unread,
    )
}

/// Delivered-but-unacked messages for a session: the recovery view for a
/// drain interrupted before it finished acting on everything. Conversation-
/// aware for the same reason `inbox` is. Read-only, unlike `inbox`, so
/// checking it never counts as processing.
pub fn pending(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    across_pane(
        conn,
        session_id,
        reach::pending_ack_for_conversation,
        reach::pending_ack,
    )
}

/// Acked messages for a session since `since_ms`: "what did that say again",
/// not a work queue. Same conversation-aware dispatch as `inbox`/`pending`.
pub fn history(
    conn: &Connection,
    session_id: &str,
    since_ms: tp_core::Millis,
) -> Result<Vec<Message>> {
    across_pane(
        conn,
        session_id,
        |c, id| reach::acked_since_for_conversation(c, id, since_ms),
        |c, sid| reach::acked_since(c, sid, since_ms),
    )
}

/// Confirm a message finished being acted on; distinct from `mark_read`, which
/// fires the instant a message is shown. Returns the timestamp it used so a
/// caller holding the `Message` can update its own copy instead of re-querying.
pub fn ack(conn: &Connection, id: &str) -> Result<tp_core::Millis> {
    let now = now_ms();
    reach::ack(conn, id, now)?;
    Ok(now)
}

/// Look a message up by an id prefix: user-facing output prints only the
/// first 8 chars of an id, so that is the only handle a caller has. An
/// ambiguous prefix is an error rather than a silent first-match: replying to
/// the wrong conversation is worse than being told to be more specific.
pub fn get_by_prefix(conn: &Connection, prefix: &str) -> Result<Message> {
    let mut rows = reach::by_prefix(conn, prefix)?;
    match rows.len() {
        0 => anyhow::bail!("no message with id starting {prefix:?}"),
        1 => Ok(rows.remove(0)),
        _ => anyhow::bail!("message id {prefix:?} is ambiguous — use more characters of the id"),
    }
}

/// Mark a message read (the target drained it).
pub fn mark_read(conn: &Connection, id: &str) -> Result<()> {
    reach::mark_read(conn, id, now_ms())
}

/// Record a wake attempt. Returns true if the message is now parked as dead
/// (past `MAX_DELIVER`), in which case the caller stops waking it.
pub fn record_wake(conn: &Connection, id: &str) -> Result<bool> {
    reach::bump_attempt(conn, id, now_ms(), MAX_DELIVER)
}

/// Messages a session should wake for (undelivered, not dead, not read).
pub fn wakeable(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    reach::wakeable(conn, session_id)
}

/// Re-exported from `tp-core`, where the single copy lives.
pub fn now_ms() -> tp_core::Millis {
    tp_core::now_ms()
}
