//! Pairing state machine.
//!
//! Device identity = ed25519 keypair fingerprint (the `device_id`). The
//! machine table is keyed by that device id. First contact must be approved
//! once by a human; afterwards peers are trusted and requests are signed.

use anyhow::{bail, Result};
use ed25519_dalek::VerifyingKey;
use rusqlite::{params, Connection};
use tp_db::query;

/// Every state a relationship can be in. There is no negative state: refusing
/// a peer deletes the row, so "not trusted" is spelled "absent".
///
/// Tombstones (`Rejected`/`Revoked`) would cost two states and a second
/// unlock step while carrying no when or why, so they deliver none of the
/// audit value that would justify them. SSH's `authorized_keys`, WireGuard's
/// peer list and Bluetooth's "Forget This Device" take the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    PendingOut,
    PendingIn,
    Trusted,
}

impl PairingStatus {
    /// The stored spelling; `trust_to_status` is the only way back, so the set
    /// of strings this column may hold is stated once here and once in the
    /// migration's CHECK. `self` is deliberately absent: that is this machine's
    /// own row (written by `ensure_self_machine` in tp-db), not a pairing state.
    pub fn as_str(self) -> &'static str {
        match self {
            PairingStatus::PendingOut => "pending_out",
            PairingStatus::PendingIn => "pending_in",
            PairingStatus::Trusted => "trusted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PairingResult {
    pub status: PairingStatus,
}

/// How many unapproved incoming requests may sit in the table at once.
///
/// This protects the approval step, not the disk. Safety rests on a human
/// reading `fl pair list` and comparing a fingerprint, and a list of
/// thousands denies that control as surely as taking the endpoint down,
/// while a lookalike beside the real name turns a misclick into a trust
/// decision. Reachable unauthenticated and keyed by `fingerprint(pubkey)`,
/// so a row costs an attacker one keygen.
pub const MAX_PENDING_IN: usize = 32;

/// The longest peer name this machine will store.
///
/// Names come from `hostname`, so this is several times any real one; it is a
/// bound on a hostile input, not a style rule.
pub const MAX_NAME_CHARS: usize = 64;

/// What `record_incoming` did.
///
/// `ListFull` is a value rather than an `Err` because the caller acts on it
/// differently: a full list is a 503 whose fix is `fl pair reject`, a database
/// failure is a 500 whose fix is nothing the caller can do. Merged behind one
/// error type, "someone is flooding you" would read as "your disk is broken".
#[derive(Debug, Clone)]
pub enum Incoming {
    Recorded(PairingResult),
    /// `MAX_PENDING_IN` is reached and this device is not already in the list.
    ListFull,
}

/// Whether a peer's self-declared name is safe to put in front of a human.
///
/// A name arrives from unauthenticated places (`/v1/pair/request`, the
/// `/v1/ping` reply) and has one use: being printed beside a fingerprint in
/// `fl pair list` while someone decides whether to trust it. So it must not
/// lie about its own shape — an ANSI escape repaints its line, a newline
/// forges a row, a bidi override reorders what is displayed.
///
/// Rejected rather than sanitised: stripping would map two distinct names
/// onto one display string, and telling peers apart by eye is this text's
/// whole job. A name may still simply be false — the device id beneath it is
/// what is actually compared — and homoglyphs cannot be handled here.
pub fn name_is_displayable(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(|c| {
            // `is_control` is category Cc. The rest is all of category Cf:
            // each renders as nothing or reorders its neighbours, so any of
            // them gives two distinct names one display string. Enumerated
            // because no dependency here carries Unicode categories; the list
            // must stay whole, since one omission is one lookalike.
            c.is_control()
                || matches!(c,
                    '\u{00ad}'
                    | '\u{0600}'..='\u{0605}'
                    | '\u{061c}'
                    | '\u{06dd}'
                    | '\u{070f}'
                    | '\u{0890}'..='\u{0891}'
                    | '\u{08e2}'
                    | '\u{180e}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{206f}'
                    | '\u{feff}'
                    | '\u{fff9}'..='\u{fffb}'
                    | '\u{110bd}'
                    | '\u{110cd}'
                    | '\u{13430}'..='\u{1343f}'
                    | '\u{1bca0}'..='\u{1bca3}'
                    | '\u{1d173}'..='\u{1d17a}'
                    | '\u{e0001}'
                    | '\u{e0020}'..='\u{e007f}')
        })
}

fn trust_to_status(trust: &str) -> PairingStatus {
    match trust {
        "pending_out" => PairingStatus::PendingOut,
        "pending_in" => PairingStatus::PendingIn,
        "trusted" => PairingStatus::Trusted,
        _ => PairingStatus::PendingOut,
    }
}

fn upsert_peer(
    conn: &Connection,
    device_id: &str,
    name: &str,
    pubkey: &VerifyingKey,
    trust: PairingStatus,
    addr: Option<&str>,
) -> Result<PairingResult> {
    // Held here, not in the callers, because it protects the write: both
    // paths in carry a name from an unauthenticated stranger, and a future
    // caller must not be able to skip the guard. `pair_request` checks
    // separately so the wire answer is a 400 rather than a 500.
    if !name_is_displayable(name) {
        bail!(
            "{device_id} sent an unusable name ({} chars): a peer name must be \
             1-{MAX_NAME_CHARS} characters and free of control or text-direction \
             characters",
            name.chars().count()
        );
    }
    // Milliseconds (0014), not the seconds `Utc::now().timestamp()` yields.
    let now = tp_core::now_ms().get();
    conn.execute(
        "INSERT INTO machine(id, name, pubkey, trust, addr, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             pubkey = excluded.pubkey,
             -- keep a known address if this path has none to offer
             addr = COALESCE(excluded.addr, machine.addr),
             trust = excluded.trust
         -- The WHERE is the guard, so the write is protected regardless of
         -- caller. This path is reached unauthenticated (first contact cannot
         -- demand a trusted signature), and device_id and pubkey are both
         -- public, so the endpoint's fingerprint check proves the key is real,
         -- never that the caller owns it. A trusted peer's row must therefore
         -- be inert here (no downgrade, no repointed addr, no rename), and so
         -- must this machine's own row: 'self' is not 'trusted'.
         WHERE machine.trust != 'trusted' AND machine.is_self = 0",
        params![
            device_id,
            name,
            pubkey.as_bytes().to_vec(),
            trust.as_str(),
            addr,
            now
        ],
    )?;
    let row = query::machine(conn, device_id)?.expect("just inserted");
    Ok(PairingResult {
        status: trust_to_status(&row.trust),
    })
}

/// I initiated a pairing with `device_id`. Records `pending_out`.
///
/// `addr` is where I reached them — this is the only path that learns a
/// peer's address from an outgoing action, and without storing it here a
/// trusted peer would have no address to fan out to later.
pub fn request_out(
    conn: &Connection,
    device_id: &str,
    name: &str,
    pubkey: &VerifyingKey,
    addr: Option<&str>,
) -> Result<PairingResult> {
    upsert_peer(
        conn,
        device_id,
        name,
        pubkey,
        PairingStatus::PendingOut,
        addr,
    )
}

/// I received a pairing request from `device_id`. Records `pending_in`.
///
/// `addr` is the socket the request arrived from, so an approved peer is
/// immediately reachable without waiting for an mDNS round.
pub fn record_incoming(
    conn: &Connection,
    device_id: &str,
    name: &str,
    pubkey: &VerifyingKey,
    addr: Option<&str>,
) -> Result<Incoming> {
    // Only a device with no row at all can be refused. One already in the list
    // must still be able to update — a peer retrying after a restart re-sends
    // the same fingerprint, and locking it out of its own row would turn a
    // full table into a permanent one. An already-trusted device is inert here
    // anyway (see the WHERE clause in `upsert_peer`), so it is never refused
    // for a list it is not in.
    if query::machine(conn, device_id)?.is_none() && pending_in_count(conn)? >= MAX_PENDING_IN {
        return Ok(Incoming::ListFull);
    }
    upsert_peer(
        conn,
        device_id,
        name,
        pubkey,
        PairingStatus::PendingIn,
        addr,
    )
    .map(Incoming::Recorded)
}

/// Only `pending_in` counts against the cap. `pending_out` rows exist because
/// THIS operator ran `fl pair request`, so counting them would let one's own
/// outgoing attempts eat the budget for incoming ones — a stranger's flood
/// would not be limited any harder, and the operator would be.
fn pending_in_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM machine WHERE trust = 'pending_in'",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Approve a pending pairing (must be pending_out or pending_in first).
pub fn approve(conn: &Connection, device_id: &str) -> Result<PairingResult> {
    let row = query::machine(conn, device_id)?;
    let Some(row) = row else {
        bail!("no pending pairing for {device_id}");
    };
    if row.trust != "pending_out" && row.trust != "pending_in" {
        bail!(
            "no pending pairing for {device_id} (current: {})",
            row.trust
        );
    }
    // Milliseconds (0014), not the seconds `Utc::now().timestamp()` yields.
    let now = tp_core::now_ms().get();
    conn.execute(
        "UPDATE machine SET trust = 'trusted', paired_at = ?1 WHERE id = ?2",
        params![now, device_id],
    )?;
    Ok(PairingResult {
        status: PairingStatus::Trusted,
    })
}

/// Refuse a pairing that was never approved. Deletes the row: "not trusted"
/// is spelled "absent" (see `PairingStatus`), so the device is free to ask
/// again later and a human is free to say yes then.
///
/// Not for a peer that is trusted — that is `revoke`. The two do the same
/// thing to the database; the split catches the mistake at the moment of
/// action, since "refuse a stranger" and "throw out a machine I trusted"
/// deserve different words and each guard makes the wrong one an error.
pub fn reject(conn: &Connection, device_id: &str) -> Result<()> {
    let Some(row) = query::machine(conn, device_id)? else {
        // Same guard as `approve`: rejecting nothing must not report a
        // decision that never happened.
        bail!("no pairing request from {device_id} to reject");
    };
    if row.trust == "trusted" {
        bail!("{device_id} is trusted — use `fl pair revoke` to take that back, not reject");
    }
    delete_peer(conn, device_id, &row.trust)
}

/// Take back trust from a peer that currently has it. Deletes the row.
///
/// Purely local, and effective on the peer's next request: nothing is
/// cached, so `lookup_trusted_pubkey` finds no key. There is no network route
/// to tell the peer — a refusal on its next request is the guarantee this
/// machine can keep, and a notification is not.
///
/// The peer's own database still says it trusts us. That asymmetry is
/// deliberate: a 401 is forgeable by anyone on the path, so treating one as
/// "they revoked me" would let a network attacker tear down trust.
pub fn revoke(conn: &Connection, device_id: &str) -> Result<()> {
    let Some(row) = query::machine(conn, device_id)? else {
        bail!("no relationship with {device_id} to revoke");
    };
    if row.trust != "trusted" {
        bail!(
            "{device_id} is not trusted (current: {}) — nothing to revoke; \
             use `fl pair reject` for a pending request",
            row.trust
        );
    }
    delete_peer(conn, device_id, &row.trust)
}

/// The shared removal, with the one guard neither caller may skip.
///
/// `self` lives in this same table and this same column, so a device id that
/// happens to be our own would otherwise delete this machine's identity row
/// out from under the daemon. Held here rather than in each caller because
/// it protects the DELETE, and a third caller must not be able to add itself
/// without it.
fn delete_peer(conn: &Connection, device_id: &str, trust: &str) -> Result<()> {
    if trust == "self" {
        bail!("{device_id} is this machine");
    }
    conn.execute("DELETE FROM machine WHERE id = ?1", [device_id])?;
    Ok(())
}

#[cfg(test)]
mod name_tests {
    use super::name_is_displayable;

    #[test]
    fn invisible_characters_are_rejected() {
        // A real name passes.
        assert!(name_is_displayable("kitchen-mac"));

        // Zero-width and invisible formatting: each renders as nothing, so a
        // name carrying one is a second display-identical name.
        for c in [
            '\u{200b}',
            '\u{200c}',
            '\u{200d}',
            '\u{feff}',
            '\u{00ad}',
            '\u{2060}',
            '\u{180e}',
            '\u{061c}',
            '\u{fff9}',
            '\u{e0041}',
        ] {
            let name = format!("kitchen{c}-mac");
            assert!(
                !name_is_displayable(&name),
                "U+{:04X} must not be displayable",
                c as u32
            );
        }

        // The directional controls already covered stay covered.
        assert!(!name_is_displayable("kitchen\u{202e}mac"));
        assert!(!name_is_displayable("kitchen\u{2066}mac"));
        assert!(!name_is_displayable("kitchen\nmac"));
    }
}
