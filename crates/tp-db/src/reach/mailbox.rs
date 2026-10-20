//! The mailbox: `message` rows, and the read/ack pair that makes an
//! interrupted drain recoverable.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

// --------------------------------------------------------------------- message

/// A message in transit, and the A2A vocabulary it does and does not share.
///
/// The two models are inverted: in A2A the agent identity is stable and its
/// task is ephemeral; here the session id is ephemeral and the conversation
/// persists. So the mapping is offset by a level:
///
/// | A2A            | here                                    |
/// |----------------|-----------------------------------------|
/// | `AgentCard`    | the machine, keyed by ed25519           |
/// | `context_id`   | the conversation (`…/conv-<uuid>`)      |
/// | `task`         | one delivery and the reply it is waiting for |
/// | `message_id`   | `id` (the wire name; see `message_json`) |
/// | `InputRequired`| delivered, awaiting a reply             |
/// | `Completed`    | a `reply` arrived                       |
/// | `Failed`       | `dead_at`: delivery gave up             |
///
/// Two things deliberately do not map. `kind` is not A2A's `role`: role says
/// who spoke, `kind` says what is expected of the reader (`ask` wants an
/// answer, `note` does not, `reply` is one). `body` is not A2A's `parts`:
/// `parts` is a typed array and this is one string, and changing that is a
/// data-model decision, not a rename. Nothing here speaks A2A; the alignment
/// makes adding it a thin adapter.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub to_session: String,
    pub from_session: Option<String>,
    pub from_machine: String,
    pub kind: String, // ask | reply | note
    pub body: String,
    pub reply_to: Option<String>,
    pub created_at: tp_core::Millis,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub attempts: i64,
    pub dead_at: Option<i64>,
    /// Set only by an explicit `ack`, never by being shown.
    /// `read_at.is_some() && acked_at.is_none()` is "delivered, not confirmed
    /// finished": the state a caller interrupted mid-processing leaves behind,
    /// and the one `pending_ack` exists to recover.
    pub acked_at: Option<tp_core::Millis>,
}

/// The column list every message read shares. One constant so a schema change
/// cannot leave one reader projecting a different tuple than `map_message`
/// expects; new columns are appended so the positional indices hold.
const MESSAGE_COLS: &str = "id, to_session, from_session, from_machine, kind, body, reply_to,
     created_at, delivered_at, read_at, attempts, dead_at, acked_at";

fn map_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        to_session: r.get(1)?,
        from_session: r.get(2)?,
        from_machine: r.get(3)?,
        kind: r.get(4)?,
        body: r.get(5)?,
        reply_to: r.get(6)?,
        created_at: tp_core::Millis::new(r.get(7)?),
        delivered_at: r.get(8)?,
        read_at: r.get(9)?,
        attempts: r.get(10)?,
        dead_at: r.get(11)?,
        acked_at: r.get::<_, Option<i64>>(12)?.map(tp_core::Millis::new),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn insert_message(
    conn: &Connection,
    id: &str,
    to_session: &str,
    from_session: Option<&str>,
    from_machine: &str,
    kind: &str,
    body: &str,
    reply_to: Option<&str>,
    created_at: tp_core::Millis,
) -> Result<()> {
    conn.execute(
        "INSERT INTO message(id, to_session, from_session, from_machine, kind, body, reply_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, to_session, from_session, from_machine, kind, body, reply_to, created_at.get()],
    )?;
    Ok(())
}

/// Unread messages for every session id this conversation has answered to,
/// oldest first: the drain that makes a rotated address recoverable.
///
/// Mail addressed before a compaction sits in the mailbox of an id nothing
/// drains any more. Reading the union collects it, which is why
/// `conversation_member` outlives `live_session`.
pub fn unread_for_conversation(conn: &Connection, conversation_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE read_at IS NULL
                AND to_session IN (SELECT session_id FROM conversation_member
                                    WHERE conversation_id = ?1)
              ORDER BY created_at ASC"
        ))?
        .query_map([conversation_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Unread messages for a session, oldest first.
pub fn unread(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND read_at IS NULL
              ORDER BY created_at ASC"
        ))?
        .query_map([session_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The newest messages on this machine, across every session.
///
/// Not scoped to a session: this is the operator's view, while `unread`
/// answers "what is in my mailbox" for one agent. Bounded by `limit` because
/// the table only grows.
pub fn recent_messages(conn: &Connection, limit: usize) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message ORDER BY created_at DESC LIMIT ?1"
        ))?
        .query_map([limit as i64], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Unread and not parked dead: the set a wake is for.
pub fn wakeable(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND read_at IS NULL AND dead_at IS NULL
              ORDER BY created_at ASC"
        ))?
        .query_map([session_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Delivered but not yet acked, for a session, oldest first. The recovery
/// view: a message here was shown once and nothing has since confirmed it was
/// acted on.
pub fn pending_ack(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND read_at IS NOT NULL AND acked_at IS NULL
              ORDER BY created_at ASC"
        ))?
        .query_map([session_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `pending_ack`, across every session id a conversation has answered to, for
/// the same reason as `unread_for_conversation`.
pub fn pending_ack_for_conversation(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE read_at IS NOT NULL AND acked_at IS NULL
                AND to_session IN (SELECT session_id FROM conversation_member
                                    WHERE conversation_id = ?1)
              ORDER BY created_at ASC"
        ))?
        .query_map([conversation_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Acked messages for a session since a timestamp, newest first: a lookup,
/// not a work queue, so it orders the opposite way from the pending views.
pub fn acked_since(
    conn: &Connection,
    session_id: &str,
    since_ms: tp_core::Millis,
) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND acked_at IS NOT NULL AND acked_at >= ?2
              ORDER BY acked_at DESC"
        ))?
        .query_map(params![session_id, since_ms.get()], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `acked_since`, across every session id a conversation has answered to.
pub fn acked_since_for_conversation(
    conn: &Connection,
    conversation_id: &str,
    since_ms: tp_core::Millis,
) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE acked_at IS NOT NULL AND acked_at >= ?2
                AND to_session IN (SELECT session_id FROM conversation_member
                                    WHERE conversation_id = ?1)
              ORDER BY acked_at DESC"
        ))?
        .query_map(params![conversation_id, since_ms.get()], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Up to two matches for an id prefix: enough for the caller to tell "found"
/// from "ambiguous" without fetching the whole table.
pub fn by_prefix(conn: &Connection, prefix: &str) -> Result<Vec<Message>> {
    // The prefix is caller text, not a pattern: an unescaped `%` or `_` in it
    // would widen the match, and a prefix of `%` would match every row.
    let pattern = format!(
        "{}%",
        prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE id LIKE ?1 ESCAPE '\\'
              ORDER BY created_at DESC
              LIMIT 2"
        ))?
        .query_map([&pattern], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn mark_read(conn: &Connection, id: &str, now: tp_core::Millis) -> Result<()> {
    conn.execute(
        "UPDATE message SET read_at = ?1 WHERE id = ?2 AND read_at IS NULL",
        params![now.get(), id],
    )?;
    Ok(())
}

/// Confirm a message finished being acted on. Guarded on `read_at IS NOT
/// NULL`: a message never delivered has nothing to confirm. The caller turns
/// "0 rows changed" into an error, as for `mark_read`.
pub fn ack(conn: &Connection, id: &str, now: tp_core::Millis) -> Result<()> {
    conn.execute(
        "UPDATE message SET acked_at = ?1 WHERE id = ?2 AND read_at IS NOT NULL AND acked_at IS NULL",
        params![now.get(), id],
    )?;
    Ok(())
}

pub fn attempts_of(conn: &Connection, id: &str) -> Result<i64> {
    Ok(conn
        .query_row("SELECT attempts FROM message WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0))
}

/// Count one delivery attempt and report whether that killed the message.
/// The cap is reach policy, passed in; the table does not know it.
///
/// One statement rather than read-modify-write: two delivery paths run
/// concurrently by design, and an interleaved read-then-write loses a count,
/// which is the direction that defeats dead-lettering. The dead decision sits
/// in the SQL with the increment for the same reason, and `RETURNING` reads
/// the row this statement wrote rather than whatever the next reader finds.
pub fn bump_attempt(
    conn: &Connection,
    id: &str,
    now: tp_core::Millis,
    max_deliver: i64,
) -> Result<bool> {
    let dead: Option<i64> = conn
        .query_row(
            "UPDATE message
                SET attempts     = attempts + 1,
                    delivered_at = COALESCE(delivered_at, ?2),
                    dead_at      = CASE WHEN attempts + 1 >= ?3 THEN ?2 ELSE dead_at END
              WHERE id = ?1
          RETURNING dead_at",
            params![id, now.get(), max_deliver],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(dead.is_some())
}
