//! Who is reachable: sessions on this machine, machines on the network.
//!
//! `peers` is a single call and is here anyway: adapters depend on the
//! application layer, and a rule with exceptions for small cases is a
//! convention, not a boundary.

use anyhow::Result;
use tp_db::query::MachineRow;
use tp_db::reach::LiveRow;
use tp_db::Db;

/// A live session, with the address other sessions should write to.
pub struct LiveSession {
    pub row: LiveRow,
    /// The conversation address when the session has one, its segment id
    /// otherwise. Which to publish is a rule, not formatting: a segment id
    /// copied today belongs to nobody after the target's next compaction.
    pub address: String,
    /// Where the session's process runs. A background session has no terminal
    /// to type into, so a message to it waits for its next inbox check; a
    /// sender has to know that before choosing it. `None` from `live`, which
    /// does not look; `live_detailed` does.
    pub host: Option<tp_reach::discover::Host>,
    /// Whether its transcript exists. Looked up for background sessions only:
    /// a background job that failed before it began holds none and otherwise
    /// looks like a working session. `None` where not looked up — finding a
    /// transcript means scanning every project directory, about a second each
    /// on a machine with 125k transcripts.
    pub has_conversation: Option<bool>,
}

impl LiveSession {
    /// Whether `address` is a conversation address rather than the segment id.
    /// Decided here so no renderer derives it, and labels a segment id
    /// "survives compaction".
    pub fn address_is_stable(&self) -> bool {
        self.address != self.row.session_id
    }

    /// Whether this address is a scan placeholder rather than a real session id.
    ///
    /// The process scan mints `scan-pid-<N>` for a harness that never
    /// registered, rather than guess a transcript id and attribute one
    /// conversation to another. The placeholder is deliverable (the scan knows
    /// the pid and tty) but unreadable (nothing maps it to a transcript), and
    /// has the same shape as any other address, so the caller must be told.
    pub fn address_is_synthetic(&self) -> bool {
        self.address
            .rsplit('/')
            .next()
            .is_some_and(|native| native.starts_with("scan-pid-"))
    }
}

/// Every session running right now, most recently seen first.
///
/// Cheap, and kept cheap: the menu-bar panel polls this every two seconds
/// through the local API with a 600 ms deadline. 0.3.4 looked up each
/// session's host and transcript here, which took nine seconds on a machine
/// with 740 project directories; every poll timed out and the panel reported
/// the daemon down. What only a sender needs is in `live_detailed`.
pub fn live(db: &Db) -> Result<Vec<LiveSession>> {
    tp_db::reach::list_live(db.conn())?
        .into_iter()
        .map(|row| {
            let address = tp_reach::conversation_address(db.conn(), &row.session_id)?
                .unwrap_or_else(|| row.session_id.clone());
            Ok(LiveSession {
                row,
                address,
                host: None,
                has_conversation: None,
            })
        })
        .collect()
}

/// `live`, plus where each session runs and, for a background session,
/// whether it holds a conversation — for `fl live` and `floo_live`, where a
/// sender chooses a target. Read now rather than stored at registration: a
/// session moves between a terminal and the background without registering
/// again.
pub fn live_detailed(db: &Db, machine_id: &str) -> Result<Vec<LiveSession>> {
    use tp_reach::discover::Host;
    let mut rows = live(db)?;
    let pids: Vec<i32> = rows.iter().map(|r| r.row.pid).collect();
    let hosts = tp_reach::discover::hosts_of(&pids);
    for r in &mut rows {
        let host = hosts.get(&r.row.pid).copied().unwrap_or(Host::Unknown);
        r.host = Some(host);
        if host == Host::Background {
            r.has_conversation = Some(tp_ingest::adapter::transcript_exists(
                &r.row.session_id,
                machine_id,
            ));
        }
    }
    Ok(rows)
}

/// Machines this one has a relationship with, whatever its state.
pub fn peers(db: &Db) -> Result<Vec<MachineRow>> {
    tp_db::query::all_peers(db.conn())
}

/// What probing a host found.
///
/// `answered` counts every daemon that replied, including this machine, while
/// `peers` excludes it. A caller needs both to tell "nothing is listening
/// there" from "that address is me", which is an easy probe to make by
/// accident.
pub struct Probed {
    pub peers: Vec<Discovered>,
    pub answered: usize,
}

/// A floonet daemon that answered a probe.
pub struct Discovered {
    pub device_id: String,
    pub name: String,
    pub addr: String,
    /// Whether this machine is already a peer. A stranger is shown and never
    /// stored: appearing on the network is not a relationship, and writing a
    /// row for anyone who answers would let the LAN populate the trust table.
    pub known: bool,
}

/// Probe a host and classify what answers. A failed database lookup is an
/// error, never "this machine is a stranger".
pub async fn discover(db: &Db, me: &str, host: &str) -> Result<Probed> {
    let found = tp_net::probe(host).await?;
    let answered = found.len();
    Ok(Probed {
        peers: classify_discovered(db, me, found)?,
        answered,
    })
}

/// Classify what answered a probe. Separate from `discover` and synchronous,
/// so the part with rules is testable without a network.
pub fn classify_discovered(
    db: &Db,
    me: &str,
    found: Vec<tp_net::DiscoveredPeer>,
) -> Result<Vec<Discovered>> {
    let mut out = Vec::new();
    for p in found {
        // Finding ourselves is not a discovery.
        if p.device_id == me {
            continue;
        }
        // Refreshing the address of a peer we already know is a write; a
        // stranger's is not.
        let known = tp_db::query::touch_peer(db.conn(), &p.device_id, &p.addr)?;
        out.push(Discovered {
            device_id: p.device_id,
            name: p.name,
            addr: p.addr,
            known,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("me", "TestMac").unwrap();
        db
    }

    fn seen(device_id: &str) -> tp_net::DiscoveredPeer {
        tp_net::DiscoveredPeer {
            device_id: device_id.to_string(),
            name: format!("host-{device_id}"),
            addr: "192.0.2.9:47400".to_string(),
            version: "0.1.0 (test)".to_string(),
        }
    }

    /// Seeing yourself on the network is not a discovery.
    #[test]
    fn our_own_advertisement_is_dropped() {
        let db = db();
        let out = classify_discovered(&db, "me", vec![seen("me"), seen("other")]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device_id, "other");
    }

    /// A stranger is reported and not written: anything else would let anyone
    /// on the LAN put a row in the trust table.
    #[test]
    fn a_stranger_is_reported_but_never_stored() {
        let db = db();
        let out = classify_discovered(&db, "me", vec![seen("stranger")]).unwrap();
        assert_eq!(out.len(), 1);
        assert!(!out[0].known);

        let stored: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM machine WHERE id = 'stranger'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, 0, "seeing a machine is not knowing it");
    }

    /// A session's published address prefers the conversation, because a
    /// segment id stops being deliverable at the target's next compaction.
    #[test]
    fn a_live_session_publishes_its_conversation_address() {
        let db = db();
        tp_reach::register(db.conn(), "me/claude_code/seg", 4242, None, Some("/w")).unwrap();
        let rows = live(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].address.contains("/conv-"),
            "expected the stable address, got {}",
            rows[0].address
        );
        assert_eq!(rows[0].row.session_id, "me/claude_code/seg");
        assert!(rows[0].address_is_stable());
    }

    /// A session with no conversation publishes its segment id, and says so.
    #[test]
    fn a_session_without_a_conversation_does_not_claim_stability() {
        let db = db();
        db.conn()
            .execute(
                "INSERT INTO live_session(session_id, pid, source, registered_at, last_seen_at, presence)
                 VALUES ('me/claude_code/bare', 1, 'scan', 0, 0, 'scan')",
                [],
            )
            .unwrap();
        let rows = live(&db).unwrap();
        assert_eq!(rows[0].address, "me/claude_code/bare");
        assert!(!rows[0].address_is_stable());
    }
}

#[cfg(test)]
mod cost_tests {
    use super::live;
    use tp_db::Db;

    /// `live` is polled by the menu-bar panel every two seconds with a 600 ms
    /// deadline. 0.3.4 looked up hosts and transcripts in it — nine seconds on
    /// a machine with 740 project directories — and the panel reported the
    /// daemon down. This pins `live` to doing neither; `live_detailed` is
    /// where those belong.
    #[test]
    fn the_polled_listing_does_no_host_or_transcript_lookups() {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        tp_reach::register(
            db.conn(),
            "m1/claude_code/aaaa",
            std::process::id() as i32,
            None,
            Some("/w"),
        )
        .unwrap();
        let rows = live(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].host.is_none(),
            "live must not walk process ancestry"
        );
        assert!(
            rows[0].has_conversation.is_none(),
            "live must not search for transcripts"
        );
    }
}
