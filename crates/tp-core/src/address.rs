/// How deliverable an address is.
///
/// Product vocabulary rather than a storage detail, which is why it lives here
/// and not beside the query that computes most of it: the variants exist
/// because a sender must be told different things, and every surface speaks
/// them. `live_session` and `session` answer different questions, and a sender
/// told "delivered on next inbox" for an undeliverable address has been
/// promised something with no basis, so the states stay apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressability {
    /// A live registration exists. Something is expected to drain this mailbox.
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
    /// Not registered, but a session floonet has indexed. Real once, not
    /// currently claimed by any process — most often because the id rotated
    /// (Claude Code mints a new session id at every compaction) and the
    /// conversation now answers to a different address.
    Dormant,
    /// No record of this id at all.
    ///
    /// `transcript_readable` splits the two failures that otherwise wear the
    /// same name: floonet can READ a session whose transcript is on disk while
    /// nothing is registered to receive at that address, and readable is not
    /// reachable. The flag is carried rather than re-derived because answering
    /// it needs the transcript roots rather than the database, and three
    /// surfaces used to ask that question separately.
    Unknown { transcript_readable: bool },
}
