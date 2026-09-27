//! What an address means: whether it can be delivered to, and the
//! `conversation` rows that keep it stable across compaction.
//!
//! One decision — a conversation's identity outliving the session ids it is
//! made of — which is the subtlest thing in this crate and the reason a
//! message sent an hour ago still arrives.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// What the database alone can say about an address.
///
/// Deliberately not `tp_core::Addressability`: the product answer also depends
/// on whether a transcript is readable, which is a question for the transcript
/// roots and not for any table here. tp-app composes the two, so this type
/// cannot express a claim this layer has no basis for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredAddressability {
    /// A `live_session` row exists. Something is expected to drain this mailbox.
    Registered,
    /// A conversation floonet itself published, whose members are all
    /// currently unregistered. Distinct from `Unknown`: an address floonet
    /// printed must not be reported as never seen, or a sender reads it as a
    /// rejection and resends.
    DormantConversation,
    /// A conversation whose host process is gone.
    ///
    /// Split from `DormantConversation` because the two need opposite advice.
    /// A conversation is keyed on its host pid, so once that process exits no
    /// segment will ever register into it again; the sender must look up the
    /// current address rather than wait.
    EndedConversation,
    /// No `live_session` row, but the session is one floonet has indexed. Real
    /// once, not currently claimed by any process — most often because the id
    /// rotated (Claude Code mints a new session id at every compaction) and the
    /// conversation now answers to a different address.
    Dormant,
    /// Neither table knows this id. It may be a session that has not registered
    /// or been indexed yet — including one on a peer machine — so this is not
    /// proof of a bad address, only the absence of any evidence for it.
    Unknown,
}

/// Classify an address without enqueueing anything, so a caller can say what
/// will happen before it promises delivery.
pub fn addressability(conn: &Connection, session_id: &str) -> Result<StoredAddressability> {
    let live: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_session WHERE session_id = ?1)",
        [session_id],
        |r| r.get(0),
    )?;
    if live {
        return Ok(StoredAddressability::Registered);
    }
    // Membership is the test, not the shape of the address: a conversation
    // address resolves to a member before this runs, so either form counts.
    let is_conv: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation WHERE id = ?1)
             OR EXISTS(SELECT 1 FROM conversation_member WHERE session_id = ?1)",
        [session_id],
        |r| r.get(0),
    )?;
    if is_conv {
        // Alive-but-between-registrations, or over for good? The conversation
        // records its host pid; if nothing live still runs under that pid, no
        // future segment can join, and the sender must be told to look up the
        // current address rather than to wait.
        let host_alive: bool = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM live_session l
                  WHERE l.pid = (SELECT c.pid FROM conversation c
                                  WHERE c.id = ?1
                                     OR c.id = (SELECT conversation_id FROM conversation_member
                                                 WHERE session_id = ?1))
             )",
            [session_id],
            |r| r.get(0),
        )?;
        return Ok(if host_alive {
            StoredAddressability::DormantConversation
        } else {
            StoredAddressability::EndedConversation
        });
    }
    let known: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
        [session_id],
        |r| r.get(0),
    )?;
    Ok(if known {
        StoredAddressability::Dormant
    } else {
        StoredAddressability::Unknown
    })
}
// ---------------------------------------------------------------- conversation

/// How long after a conversation was last seen a new session on the same
/// process may still be treated as its continuation.
///
/// A compaction re-registers within milliseconds, so this only has to cover a
/// slow hook. Short on purpose: pids are reused, and a generous window would
/// merge two unrelated conversations that inherited the same pid in the same
/// directory, delivering one agent's mail to another.
pub const CONVERSATION_JOIN_GRACE_MS: i64 = 5 * 60_000;

/// How a rotation is recognized. Grouped because the parts only mean anything
/// together: `pid` alone collides after reuse, `cwd` alone collides across
/// concurrent sessions, `runtime_id` alone is not an identity at all.
#[derive(Debug, Clone, Copy)]
pub struct ConversationKey<'a> {
    pub machine_id: &'a str,
    pub runtime_id: &'a str,
    pub pid: i32,
    /// Opaque process start time, `ps -o lstart=` verbatim. With the pid this
    /// names a process incarnation, which is what a conversation belongs to.
    /// `None` when it could not be read; the caller then falls back to the
    /// time window, which is weaker but never wrong-by-merge.
    pub pid_start: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

/// Every conversation row that belongs to the same pane as `session_id`.
///
/// A pane can own more than one: `join_conversation` keys on
/// `(pid, pid_start, cwd)`, `cwd` changes during ordinary work, and rows with
/// no `pid_start` cannot match one that has it. The twins are all legitimate,
/// and mail addressed to one must be visible from the other.
///
/// Ordered most-recently-seen first, so a caller that wants one (the address
/// to publish) takes the head and a caller that wants all (the mailboxes to
/// drain) takes the lot.
pub fn conversations_of_pane(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    // The pane is identified through the session's own conversation row rather
    // than by re-deriving pid/pid_start: whatever key that row was created
    // with is the key its twins share.
    let Some(mine) = conversation_of(conn, session_id)? else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT c.id FROM conversation c
           JOIN conversation me ON me.id = ?1
          WHERE c.machine_id = me.machine_id
            AND c.runtime_id = me.runtime_id
            AND c.pid        = me.pid
            AND (c.pid_start IS me.pid_start)
          ORDER BY c.last_seen_at DESC",
    )?;
    let rows = stmt
        .query_map([&mine], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // `cwd` is deliberately not in the match: it is the key component that
    // splits a pane, and `(pid, pid_start)` already names an incarnation.
    Ok(if rows.is_empty() { vec![mine] } else { rows })
}

/// Bind `session_id` to a conversation, continuing an existing one when this
/// looks like a rotation and minting a new address otherwise.
///
/// Recognition is `(runtime_id, pid, pid_start, cwd)`; when `pid_start` is
/// unknown it degrades to `(runtime_id, pid, cwd)` inside
/// `CONVERSATION_JOIN_GRACE_MS` (see `find_conversation`). Idempotent: a
/// session already bound keeps its conversation, so re-running the start hook
/// never re-parents it.
pub fn join_conversation(
    conn: &Connection,
    session_id: &str,
    key: ConversationKey<'_>,
    now: tp_core::Millis,
    new_id: &str,
) -> Result<String> {
    let ConversationKey {
        machine_id,
        runtime_id,
        pid,
        cwd,
        ..
    } = key;
    if let Some(existing) = conversation_of(conn, session_id)? {
        conn.execute(
            "UPDATE conversation SET last_seen_at = ?2, pid = ?3 WHERE id = ?1",
            params![existing, now.get(), pid],
        )?;
        return Ok(existing);
    }

    let continues = find_conversation(conn, key, now)?;

    let conv = match continues {
        Some(id) => {
            conn.execute(
                "UPDATE conversation SET last_seen_at = ?2, cwd = COALESCE(?3, cwd) WHERE id = ?1",
                params![id, now.get(), cwd],
            )?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO conversation(id, machine_id, runtime_id, pid, pid_start, cwd, created_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![new_id, machine_id, runtime_id, pid, key.pid_start, cwd, now.get()],
            )?;
            new_id.to_string()
        }
    };

    conn.execute(
        "INSERT INTO conversation_member(session_id, conversation_id, joined_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, conv, now.get()],
    )?;
    Ok(conv)
}

/// Bind a session id the scan inferred to a conversation that already exists
/// on that process, never minting one.
///
/// The scan guesses a session id. The guess is good enough to publish as an
/// address (a wake sent to it lands on this process) but not good enough to
/// seed a new correspondent identity. Joining an existing one is required: an
/// id floonet publishes as reachable must be drainable, or it is wakeable but
/// never read.
///
/// Also refreshes `last_seen_at`, so a process that runs for hours without
/// compacting stays inside the join grace window.
pub fn join_existing_conversation(
    conn: &Connection,
    session_id: &str,
    key: ConversationKey<'_>,
    now: tp_core::Millis,
) -> Result<Option<String>> {
    if let Some(existing) = conversation_of(conn, session_id)? {
        conn.execute(
            "UPDATE conversation SET last_seen_at = ?2 WHERE id = ?1",
            params![existing, now.get()],
        )?;
        return Ok(Some(existing));
    }
    let Some(conv) = find_conversation(conn, key, now)? else {
        return Ok(None);
    };
    conn.execute(
        "INSERT INTO conversation_member(session_id, conversation_id, joined_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, conv, now.get()],
    )?;
    conn.execute(
        "UPDATE conversation SET last_seen_at = ?2 WHERE id = ?1",
        params![conv, now.get()],
    )?;
    Ok(Some(conv))
}

/// The conversation already running on this process, if any.
///
/// Two rules, chosen by whether the start time is known on both sides. Both
/// known: match on the incarnation `(runtime, pid, pid_start, cwd)` with no
/// time bound, since the fact comes from the OS and cannot go stale. Either
/// unknown: fall back to `(runtime, pid, cwd)` inside the grace window. Weaker
/// but never wrong-by-merge: a stale window can only refuse a join, which
/// mints a fresh address rather than delivering to the wrong correspondent.
fn find_conversation(
    conn: &Connection,
    key: ConversationKey<'_>,
    now: tp_core::Millis,
) -> Result<Option<String>> {
    if let Some(start) = key.pid_start {
        let exact: Option<String> = conn
            .query_row(
                "SELECT id FROM conversation
                  WHERE runtime_id = ?1 AND pid = ?2 AND pid_start = ?3 AND cwd IS ?4
                  ORDER BY last_seen_at DESC LIMIT 1",
                params![key.runtime_id, key.pid, start, key.cwd],
                |r| r.get(0),
            )
            .optional()?;
        if exact.is_some() {
            return Ok(exact);
        }
    }
    conn.query_row(
        "SELECT id FROM conversation
          WHERE runtime_id = ?1 AND pid = ?2 AND cwd IS ?3
            AND pid_start IS NULL
            AND last_seen_at >= ?4
          ORDER BY last_seen_at DESC LIMIT 1",
        params![
            key.runtime_id,
            key.pid,
            key.cwd,
            now.saturating_sub_ms(CONVERSATION_JOIN_GRACE_MS).get()
        ],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn conversation_of(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT conversation_id FROM conversation_member WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Which session a conversation currently answers on: its newest member that
/// still has a `live_session` row, falling back to the newest member at all.
///
/// The fallback matters: a message to a conversation whose current segment is
/// momentarily unregistered still lands where its next drain will find it,
/// rather than being refused.
pub fn conversation_current_session(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Option<String>> {
    let live: Option<String> = conn
        .query_row(
            "SELECT m.session_id FROM conversation_member m
               JOIN live_session l ON l.session_id = m.session_id
              WHERE m.conversation_id = ?1
              ORDER BY m.joined_at DESC LIMIT 1",
            [conversation_id],
            |r| r.get(0),
        )
        .optional()?;
    if live.is_some() {
        return Ok(live);
    }
    conn.query_row(
        "SELECT session_id FROM conversation_member
          WHERE conversation_id = ?1 ORDER BY joined_at DESC LIMIT 1",
        [conversation_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}
