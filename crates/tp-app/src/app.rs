//! The operations, with one object to reach them through.
//!
//! `App` owns the database handle so that this layer is a chokepoint: a caller
//! holding `&Db` can do anything the database allows, and a layer callers can
//! reach around cannot enforce a check, only offer one. `App` adds no policy of
//! its own; it makes policy possible.
//!
//! It owns the `Retrieval` for the same reason: provider selection has one
//! owner, or the CLI and the daemon answer the same read differently.

use crate::{fanout, inbox, pair, peers, read, send, session};
use anyhow::{Context as _, Result};
use tp_db::Db;
use tp_search::{Retrieval, ScanProvider};
/// Keys in the `setting` table. Namespaced by dot like the local API's
/// capabilities, so a reader of either can guess the other.
const PEER_LISTEN_KEY: &str = "peer.listen.enabled";
const PEER_PORT_KEY: &str = "peer.listen.port";

/// What the daemon should do about the peer port, and what the panel should say.
#[derive(Debug, Clone, Copy)]
pub struct PeerListen {
    pub enabled: bool,
    /// The port that would be used: environment, else configured, else default.
    pub port: u16,
    /// What the user configured, if anything. `None` means the default is in
    /// force, which the panel shows differently from a chosen port.
    pub configured_port: Option<u16>,
}

pub struct App {
    db: Db,
    /// Built on first use: `ScanProvider::new` re-reads every descriptor under
    /// `runtimes.d/`, and most commands never perform a read. The provider
    /// choice is still fixed at construction; only the work is deferred.
    retrieval: std::cell::OnceCell<std::sync::Arc<Retrieval>>,
    identity: tp_net::Identity,
    hostname: String,
}

impl App {
    /// The one place a database is opened.
    ///
    /// Takes paths rather than reading them from the environment: the daemon,
    /// the CLI and the tests resolve them differently, and that must stay
    /// visible at the call site.
    pub fn open(db_path: &std::path::Path, key_path: &std::path::Path) -> Result<Self> {
        let identity = tp_net::Identity::load_or_create(key_path)?;
        let db = Db::open(db_path)?;
        Ok(Self {
            db,
            retrieval: std::cell::OnceCell::new(),
            identity,
            hostname: tp_net::identity::hostname(),
        })
    }

    /// For a caller that already holds the pieces, chiefly tests with an
    /// in-memory database and a generated identity. The "one `Db::open`" rule
    /// is about who opens the file; this takes a handle that is already open.
    /// The `Retrieval` is supplied so a test can use a fixture provider.
    pub fn from_parts(db: Db, identity: tp_net::Identity, retrieval: Retrieval) -> Self {
        let cell = std::cell::OnceCell::new();
        let _ = cell.set(std::sync::Arc::new(retrieval));
        Self {
            db,
            retrieval: cell,
            hostname: tp_net::identity::hostname(),
            identity,
        }
    }

    pub fn machine_id(&self) -> &str {
        &self.identity.device_id
    }
    pub fn hostname(&self) -> &str {
        &self.hostname
    }
    pub fn identity(&self) -> &tp_net::Identity {
        &self.identity
    }
    /// An `Arc` rather than a reference: the peer server clones it out of its
    /// lock before `spawn_blocking`, so a long scan does not hold the `App`
    /// borrowed and block ping and pairing behind a search.
    pub fn retrieval(&self) -> std::sync::Arc<Retrieval> {
        self.retrieval
            .get_or_init(|| std::sync::Arc::new(build_retrieval(&self.identity.device_id)))
            .clone()
    }

    /// Record what is actually serving, so `fl version` and the panel can tell
    /// a stale daemon from a current one: installing binaries does not restart
    /// a LaunchAgent. Not done by `open`, which must stay read-only.
    pub fn record_daemon_start(&self, version: &str, pid: u32) -> Result<()> {
        self.db.record_daemon_start(version, pid)
    }

    pub fn ensure_self_machine(&self) -> Result<()> {
        self.db
            .ensure_self_machine(&self.identity.device_id, &self.hostname)
    }

    // ── inbox ────────────────────────────────────────────────────────────────
    pub fn drain(&self, session_id: &str) -> Result<inbox::Drained> {
        inbox::drain(&self.db, session_id)
    }
    pub fn pending(&self, session_id: &str) -> Result<Vec<tp_reach::Message>> {
        inbox::pending(&self.db, session_id)
    }
    pub fn history(
        &self,
        session_id: &str,
        since: tp_core::Millis,
    ) -> Result<Vec<tp_reach::Message>> {
        inbox::history(&self.db, session_id, since)
    }
    pub fn ack(&self, message_id: &str) -> Result<tp_reach::Message> {
        inbox::ack(&self.db, message_id)
    }
    pub fn own_session(&self, pid: i32) -> Result<tp_reach::OwnSession> {
        inbox::own_session(&self.db, pid)
    }

    /// The most recent messages on this machine, newest first, across every
    /// session: the operator's view. `history` is one session's thread and is
    /// what an agent reads.
    pub fn recent_messages(&self, limit: usize) -> Result<Vec<tp_reach::Message>> {
        tp_db::reach::recent_messages(self.db.conn(), limit)
    }

    /// Whether this machine listens for peers, and on which port.
    ///
    /// Off unless the user turned it on: the listener is the one thing that
    /// makes an otherwise local program reachable from outside, and a default
    /// is not something the user added. Pairing does not need it; `fl pair` is
    /// a local operation, so a machine can be paired first and listen after.
    pub fn peer_listen(&self) -> Result<PeerListen> {
        // A read error is not "off". Off must mean chosen or absent, never
        // "the question could not be asked".
        let conn = self.db.conn();
        let on = tp_db::query::setting(conn, PEER_LISTEN_KEY)?.is_some_and(|v| v == "1");
        let port = tp_db::query::setting(conn, PEER_PORT_KEY)?.and_then(|v| v.parse::<u16>().ok());
        Ok(PeerListen {
            enabled: on,
            port: tp_net::serve_port_with(port),
            configured_port: port,
        })
    }

    pub fn set_peer_listen(&self, enabled: bool, port: Option<u16>) -> Result<()> {
        let conn = self.db.conn();
        let now = tp_core::now_ms();
        tp_db::query::set_setting(conn, PEER_LISTEN_KEY, if enabled { "1" } else { "0" }, now)?;
        if let Some(p) = port {
            tp_db::query::set_setting(conn, PEER_PORT_KEY, &p.to_string(), now)?;
        }
        Ok(())
    }

    /// A human name for a working directory, shown instead of a long path.
    /// Owned here rather than by the panel, so the schema has one owner.
    pub fn terminal_alias(&self, cwd: &str) -> Result<Option<String>> {
        tp_db::query::terminal_alias(self.db.conn(), cwd)
    }
    pub fn set_terminal_alias(&self, cwd: &str, alias: &str) -> Result<()> {
        tp_db::query::set_terminal_alias(self.db.conn(), cwd, alias, tp_core::now_ms())
    }

    // ── send ─────────────────────────────────────────────────────────────────
    pub fn send(
        &self,
        address: &str,
        message: &str,
        kind: send::Kind,
        from: Option<String>,
    ) -> Result<send::Sent> {
        send::send(&self.db, self.machine_id(), address, message, kind, from)
    }
    pub fn reply(
        &self,
        message_id: &str,
        message: &str,
        from: Option<String>,
    ) -> Result<send::Sent> {
        send::reply(&self.db, self.machine_id(), message_id, message, from)
    }

    // ── peers / pairing ──────────────────────────────────────────────────────
    pub fn live(&self) -> Result<Vec<peers::LiveSession>> {
        peers::live(&self.db)
    }
    /// One peer by device id: what `/v1/ping` answers with, and what the
    /// signature verifier looks up a key by.
    pub fn machine(&self, device_id: &str) -> Result<Option<tp_db::query::MachineRow>> {
        tp_db::query::machine(self.db.conn(), device_id)
    }
    /// Peers this machine will answer. Excludes self by construction
    /// (`is_self = 0`), which is what stops a machine pairing with itself.
    pub fn trusted_peers(&self) -> Result<Vec<tp_db::query::MachineRow>> {
        tp_db::query::trusted_peers(self.db.conn())
    }
    pub fn peers(&self) -> Result<Vec<tp_db::query::MachineRow>> {
        peers::peers(&self.db)
    }
    pub fn classify_discovered(
        &self,
        found: Vec<tp_net::DiscoveredPeer>,
    ) -> Result<Vec<peers::Discovered>> {
        peers::classify_discovered(&self.db, self.machine_id(), found)
    }
    /// Probe and classify in one call: the network half lives in
    /// `tp_net::probe`, the classification half needs the database, and keeping
    /// them together is what stops a caller from storing a stranger.
    pub async fn discover(&self, host: &str) -> Result<peers::Probed> {
        peers::discover(&self.db, self.machine_id(), host).await
    }
    pub async fn pair_request(&self, addr: &str, my_port: u16) -> Result<pair::Requested> {
        pair::request(&self.db, &self.identity, addr, &self.hostname, my_port).await
    }
    /// A stranger introducing itself over `/v1/pair/request`. The one write a
    /// not-yet-trusted peer can cause, and it can only produce a `pending_in`
    /// row: approval is `fl pair approve`, and there is no network path to it.
    pub fn record_incoming_pairing(
        &self,
        device_id: &str,
        name: &str,
        pubkey: &tp_net::VerifyingKey,
        addr: Option<&str>,
    ) -> Result<tp_net::pairing::Incoming> {
        tp_net::pairing::record_incoming(self.db.conn(), device_id, name, pubkey, addr)
    }

    pub fn pairings(&self) -> Result<pair::Pairings> {
        pair::pairings(&self.db)
    }
    pub fn pair_decide(
        &self,
        device_id: &str,
        accept: bool,
    ) -> Result<Option<tp_net::pairing::PairingStatus>> {
        pair::decide(&self.db, device_id, accept)
    }
    pub fn pair_revoke(&self, device_id: &str) -> Result<()> {
        pair::revoke(&self.db, device_id)
    }
    pub fn fanout_select(&self, only: &[String]) -> Result<fanout::Fanout> {
        fanout::select(&self.db, only)
    }

    // ── session registry ─────────────────────────────────────────────────────
    pub fn register(
        &self,
        session_id: &str,
        host: session::Host,
        cwd: Option<&str>,
        presence: tp_reach::resolve::Presence,
        deliver: Option<&str>,
    ) -> Result<session::Registered> {
        session::register(&self.db, session_id, host, cwd, presence, deliver)
    }
    pub fn unregister(&self, session_id: &str, expected_pid: Option<i32>) -> Result<bool> {
        session::unregister(&self.db, session_id, expected_pid)
    }
    pub fn heartbeat(&self, session_id: &str) -> Result<bool> {
        session::heartbeat(&self.db, session_id)
    }

    pub fn addressability(&self, target: &str) -> Result<tp_core::Addressability> {
        addressability_of(&self.db, self.machine_id(), target)
    }
    pub fn conversations_of_pane(&self, session_id: &str) -> Result<Vec<String>> {
        tp_reach::conversations_of_pane(self.db.conn(), session_id)
    }
    pub fn session_of_process(&self, pid: i32) -> Result<Option<String>> {
        tp_reach::session_of_process(self.db.conn(), pid)
    }
    pub fn runtimes_for_native(&self, bare: &str) -> Result<Vec<String>> {
        tp_db::reach::runtimes_for_native(self.db.conn(), bare)
    }
    /// The one operation here with an effect on another session, and the first
    /// thing any future policy would guard. `Caller` is a parameter rather than
    /// a property of the `App`: the CLI and the MCP server share one `App`
    /// shape, and a check that cannot tell them apart cannot express "not from
    /// a model".
    pub fn attempt_wake(
        &self,
        session_id: &str,
        control: &str,
        caller: tp_reach::Caller,
    ) -> Result<tp_reach::DeliveryOutcome> {
        tp_reach::attempt_wake(self.db.conn(), session_id, control, caller)
    }

    // ── runtime descriptors ──────────────────────────────────────────────────
    //
    // Only this descriptor call lives on App. Pure string lookups such as
    // `process_signature_for` stay direct calls; they do not earn an `&App`.
    pub fn descriptor_overrides(&self) -> Vec<tp_ingest::adapter::DescriptorOverride> {
        tp_ingest::adapter::descriptor_overrides()
    }

    // ── presence, as the daemon's cycle drives it ────────────────────────────
    //
    // These are operations, not machinery: they change state the panel reads.
    // `fld` is App's host; its timers schedule them. Debounce and file-event
    // draining stay in the daemon, observable by nothing outside it.

    /// Mark then evict declared sessions past their heartbeat TTL.
    /// Returns `(marked, evicted)`.
    pub fn sweep_declared(&self) -> Result<(usize, usize)> {
        tp_reach::sweep_declared(self.db.conn())
    }

    /// Reconcile `live_session` against what a process scan found. `empty`
    /// says whether an empty scan is authoritative: a scan that could not look
    /// must not evict every live row.
    pub fn reconcile_scan(
        &self,
        found: &[tp_reach::discover::ScannedProcess],
        empty: tp_reach::discover::EmptyScan,
    ) -> Result<()> {
        tp_reach::reconcile(self.db.conn(), self.machine_id(), found, empty)
    }

    /// Evict registrations for sessions that were never conversations.
    ///
    /// The register-time check cannot do this alone: SessionStart fires before
    /// the transcript has a first line, and "cannot tell" must mean "register",
    /// so some non-conversations always get in. Runs once at daemon startup,
    /// when the file is written and the answer is available; per-cycle would
    /// pay a file read per hook row forever to clean a handful of rows a day.
    ///
    /// A ghost row matters because it carries a pane's pid and tty: `fl ask`
    /// to it wakes a terminal whose agent finds nothing, and the sender is told
    /// "delivered". Returns how many it removed, and never fails the caller: an
    /// unreadable transcript must not stop the daemon.
    pub fn sweep_non_conversations(
        &self,
        judged: &mut std::collections::HashSet<String>,
    ) -> Result<usize> {
        let mut removed = 0;
        for (session_id, runtime_id) in tp_db::reach::hook_sessions(self.db.conn())? {
            if judged.contains(&session_id) {
                continue;
            }
            let kinds = tp_ingest::adapter::non_conversation_types_for(&runtime_id);
            if kinds.is_empty() {
                judged.insert(session_id);
                continue;
            }
            // Unreadable means "cannot tell", and cannot-tell never evicts: a
            // wrongly kept row is noise, a wrongly removed one is unreachable.
            //
            // And cannot-tell is never cached. It is the state every session
            // passes through between SessionStart and the harness writing the
            // transcript's first line; recording it as a verdict would leave
            // the sweep permanently blind to the newest rows, which are the
            // ones it exists to remove.
            let Some(kind) = tp_ingest::adapter::transcript_kind_of(&session_id) else {
                continue;
            };
            if kinds.contains(&kind) {
                removed += tp_db::reach::delete_session(self.db.conn(), &session_id)?;
            } else {
                judged.insert(session_id);
            }
        }
        Ok(removed)
    }

    // ── daemon / storage status ──────────────────────────────────────────────
    pub fn daemon_status(&self) -> Result<Option<tp_db::query::DaemonStatus>> {
        tp_db::query::daemon_status(self.db.conn())
    }
    /// How much of the index has no other copy: the number that decides whether
    /// losing this file is an inconvenience or a loss. Returns `(sessions,
    /// turns)`.
    ///
    /// A row with no `source_path` is counted: the only remaining writer is
    /// push-ingest for runtimes that keep no transcript. A row whose path still
    /// resolves is replaceable from that file.
    pub fn irreplaceable(&self) -> Result<(i64, i64)> {
        let rows = tp_db::query::sessions_with_source_paths(self.db.conn())?;
        let (mut sessions, mut turns) = (0i64, 0i64);
        for (_, path, n) in rows {
            let gone = match &path {
                Some(p) => !std::path::Path::new(p).exists(),
                None => true,
            };
            if gone {
                sessions += 1;
                turns += n;
            }
        }
        Ok((sessions, turns))
    }

    /// What `fl verify` measures, without deciding how to say it. The three
    /// structural counts are floonet's own invariants, which SQLite cannot
    /// check; `pragma_result` is SQLite's own answer, verbatim.
    pub fn verify(&self, full: bool) -> Result<Verified> {
        let c = tp_db::query::verify_counts(self.db.conn(), full)?;
        Ok(Verified {
            pragma: c.pragma,
            pragma_result: c.pragma_result,
            miscounted: c.miscounted,
            orphans: c.orphans,
            unindexed: c.unindexed,
        })
    }

    /// Turns newer than `days` ago, one of the two numbers `fl version`'s
    /// growth estimate needs. The query belongs here; the arithmetic and the
    /// wording do not.
    pub fn turns_since_days(&self, days: i64) -> Result<i64> {
        let cutoff = tp_core::now_ms().saturating_sub_ms(days * 86_400_000);
        tp_db::query::turns_since(self.db.conn(), cutoff)
    }

    pub fn oldest_turn_ms(&self) -> Result<Option<i64>> {
        tp_db::query::oldest_turn_ms(self.db.conn())
    }

    pub fn turn_count(&self) -> Result<i64> {
        tp_db::query::turn_count(self.db.conn())
    }

    /// One consistent snapshot of the database, and the bookkeeping that lets
    /// `fl version` say how old it is. `VACUUM INTO` rather than a file copy:
    /// it takes a read lock and writes the WAL-applied state, so it is safe
    /// while `fld` is writing.
    ///
    /// The record is written after the copy lands, so a failed VACUUM never
    /// claims a backup exists. A failure to record comes back as the second
    /// element rather than as an error: the backup is on disk and good, and
    /// only the bookkeeping failed.
    pub fn backup_to(&self, dest: &std::path::Path) -> Result<(i64, Option<String>)> {
        self.db
            .conn()
            .execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])
            .with_context(|| format!("writing {}", dest.display()))?;
        let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        let total = self.turn_count().unwrap_or(0);
        let record_err = self
            .db
            .record_backup(&dest.to_string_lossy(), total, size)
            .err()
            .map(|e| e.to_string());
        Ok((total, record_err))
    }

    pub fn backup_status(&self) -> Result<Option<tp_db::query::BackupStatus>> {
        tp_db::query::backup_status(self.db.conn())
    }

    // ── reads (through the owned `Retrieval`) ────────────────────────────────
    pub fn search(
        &self,
        q: &tp_core::Query,
        scope: &tp_core::Scope,
    ) -> Result<tp_core::Retrieved<tp_core::Hit>> {
        read::search(&self.retrieval(), q, scope)
    }
    pub fn sessions(
        &self,
        scope: &tp_core::Scope,
        limit: usize,
    ) -> Result<tp_core::Retrieved<tp_core::SessionRow>> {
        read::sessions(&self.retrieval(), scope, limit)
    }
    pub fn turns(
        &self,
        session_id: &str,
        cursor: tp_core::TurnCursor,
        include_thinking: bool,
        limit: usize,
        budget_bytes: Option<usize>,
    ) -> Result<tp_core::Retrieved<tp_core::turn::NormalizedTurn>> {
        read::turns(
            &self.retrieval(),
            session_id,
            cursor,
            include_thinking,
            limit,
            budget_bytes,
        )
    }
    pub fn resolve_session(
        &self,
        scope: &tp_core::Scope,
        limit: usize,
    ) -> Result<read::Resolution> {
        read::resolve_session(&self.retrieval(), scope, limit)
    }
    pub fn unscannable_note(
        &self,
        reports_unscannable: bool,
        scope: &tp_core::Scope,
    ) -> Option<String> {
        read::unscannable_note(reports_unscannable, scope, &self.db)
    }
}

/// The one place a provider stack is assembled. Free rather than a method
/// because the MCP server needs an owned second one alongside its `App`, and
/// requiring an `App` per provider would distort that path.
pub fn build_retrieval(machine_id: &str) -> Retrieval {
    Retrieval::new(Box::new(ScanProvider::new(
        machine_id.to_string(),
        tp_ingest::adapter::all_adapters(),
        tp_ingest::adapter::all_roots(),
    )))
}

/// The result of [`App::verify`]: numbers, with no opinion about wording.
/// The database's verdict, plus the one fact it cannot reach.
///
/// The only place `transcript_exists` meets an address classification. Three
/// surfaces used to ask it themselves inside their own `Unknown` arm, which is
/// three chances for the prose, the note and the token to disagree about what
/// "readable" means.
pub(crate) fn addressability_of(
    db: &Db,
    machine_id: &str,
    target: &str,
) -> Result<tp_core::Addressability> {
    use tp_db::reach::StoredAddressability as S;
    Ok(match tp_db::reach::addressability(db.conn(), target)? {
        S::Registered => tp_core::Addressability::Registered,
        S::DormantConversation => tp_core::Addressability::DormantConversation,
        S::EndedConversation => tp_core::Addressability::EndedConversation,
        S::Dormant => tp_core::Addressability::Dormant,
        S::Unknown => tp_core::Addressability::Unknown {
            transcript_readable: tp_ingest::adapter::transcript_exists(target, machine_id),
        },
    })
}

#[derive(Debug, Clone)]
pub struct Verified {
    /// Which pragma ran: `quick_check` or `integrity_check`.
    pub pragma: String,
    /// SQLite's answer, verbatim. `"ok"` is the only passing value.
    pub pragma_result: String,
    pub miscounted: i64,
    pub orphans: i64,
    pub unindexed: i64,
}

impl Verified {
    pub fn is_ok(&self) -> bool {
        self.pragma_result == "ok"
            && self.miscounted == 0
            && self.orphans == 0
            && self.unindexed == 0
    }
}
