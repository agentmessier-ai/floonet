//! The operations, with no opinion about how to report them.
//!
//! An operation here takes what it needs and returns a value. It does not
//! print, serialise, or decide what a result means. The CLI renders prose for
//! a person and the MCP server serialises data for a model; both are adapters
//! over one copy of the behaviour, so it cannot diverge between them.
//!
//! The domain crates below (`tp-db`, `tp-reach`, `tp-search`, `tp-ingest`)
//! remain where the rules live. This layer composes them into the operations
//! a user or an agent asks for; it is a coordinator, not a second home for
//! domain logic.

pub mod app;
pub mod fanout;
pub mod inbox;
pub mod pair;
pub mod peers;
pub mod read;
pub mod send;
pub mod session;

/// Re-exported because it is a public field of `Sent`: a caller must be able
/// to name the type it is handed without a direct dependency on tp-db.
pub use tp_core::Addressability;

pub use app::{build_retrieval, App, PeerListen, Verified};
pub use fanout::Fanout;
pub use inbox::{ack, drain, history, own_session, pending, Drained};
pub use pair::{Direction, Pairings, Pending};
pub use peers::{classify_discovered, discover, live, Discovered, LiveSession, Probed};
pub use read::{is_partial, resolve_session, Resolution};
pub use send::{reply, send, Kind, Sent};
pub use session::{parse_presence, Host, Registered};

/// Reject an address that is not structurally an address, before anything is
/// enqueued.
///
/// A mailbox has no FK to `session`, because it must accept an id for a
/// session floonet has not indexed yet, so "unknown" cannot be an error.
/// Malformed is different: a string that does not parse as
/// `<machine>/<runtime>/<native>` is one no future registration could make
/// valid, and storing it would lose the message with no trace of failure.
pub fn validate_address(target: &str) -> anyhow::Result<()> {
    if tp_core::SessionId::parse(target).is_some() {
        return Ok(());
    }
    anyhow::bail!(
        "{target:?} is not a session address — expected `<machine>/<runtime>/<native>`.\n\
         A bare session id, a bare machine id, or a truncated one cannot be delivered to, \
         and floonet would otherwise accept it and drop it silently.\n\
         Run `fl live` and copy an address from there; never assemble one yourself."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each shape of malformed address is refused, with advice on where a real
    /// one comes from.
    #[test]
    fn malformed_addresses_are_refused() {
        for bad in [
            "e2e0a11c-0000-4000-8000-000000000001", // bare native id
            "AAAA-BBBB-CCCC-DDDD",                  // bare machine id
            "AAAA-BBBB-CCCC-DDDD/claud",            // truncated before the native id
            "machine//native",                      // empty runtime segment
        ] {
            let err = validate_address(bad).unwrap_err().to_string();
            assert!(err.contains("is not a session address"), "{bad:?}: {err}");
            assert!(err.contains("fl live"), "{bad:?} must say where to get one");
        }
    }

    #[test]
    fn a_well_formed_address_passes_even_when_unknown() {
        // Unknown is not malformed: a mailbox accepts an id before its session
        // is indexed.
        validate_address("m/claude_code/never-seen-before").unwrap();
        // A conversation address is well-formed too; it is what `fl live`
        // publishes.
        validate_address("m/claude_code/conv-0000-1111").unwrap();
    }
}
