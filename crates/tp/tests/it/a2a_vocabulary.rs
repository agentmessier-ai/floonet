//! The words floonet puts on the wire, pinned against A2A's.
//!
//! A2A is how deployed agent services talk to each other; floonet reaches the
//! agents that are not services. The two compose cheaply only if the vocabulary
//! lines up, and vocabulary drifts silently: nothing fails when a field is
//! renamed. Nothing here speaks A2A yet. This pins the names that already agree
//! and the two that deliberately do not, so a later adapter is thin and a
//! deliberate divergence is not mistaken for an oversight.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn src(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The names A2A uses that floonet already emits, and must keep emitting.
#[test]
fn the_wire_keeps_the_names_a2a_uses() {
    let mcp = src("src/mcp.rs");
    let start = mcp
        .find("fn message_json")
        .expect("message_json is where a message reaches a model");
    let body = &mcp[start..start + 2400];

    for name in ["message_id", "context_id"] {
        assert!(
            body.contains(&format!("\"{name}\"")),
            "message_json no longer emits `{name}` — A2A's name for it, and the \
             one a downstream adapter would look for"
        );
    }

    // `context_id` is emitted only for a conversation address: its presence
    // means "survives compaction", which an unconditional copy of
    // `from_session` could not promise.
    assert!(
        body.contains("conv-"),
        "context_id must be gated on the address being a CONVERSATION address; \
         without that check its presence promises stability it cannot deliver"
    );
}

/// The two that must not be aligned. Both look like easy wins to someone
/// tidying toward a standard, and both would lose information.
#[test]
fn the_deliberate_divergences_stay_diverged() {
    let db = src("../tp-db/src/reach/mailbox.rs");

    // `kind` (ask / note / reply) is not A2A's `role` (user / agent): role says
    // who spoke; kind says what is expected of the reader.
    let msg = db
        .find("pub struct Message {")
        .map(|i| &db[i..i + 700])
        .expect("Message struct");
    assert!(
        msg.contains("pub kind: String"),
        "`kind` was renamed or retyped — if it became `role`, the ask/note \
         distinction is gone and notifications start costing a turn to answer"
    );
    assert!(
        !msg.contains("pub role:"),
        "a `role` field appeared on Message: A2A's role is a different axis \
         from `kind`, and carrying both without saying so invites using the \
         wrong one"
    );

    // The mapping must stay written down: it is the only record of why these
    // two diverge.
    let doc = db
        .find("A2A vocabulary it does and does not share")
        .map(|i| &db[i.saturating_sub(200)..i + 2600])
        .expect("the A2A mapping doc block is missing from tp-db/src/reach/mailbox.rs");
    let mut named = BTreeSet::new();
    for term in [
        "context_id",
        "message_id",
        "AgentCard",
        "InputRequired",
        "kind",
        "parts",
    ] {
        if doc.contains(term) {
            named.insert(term);
        }
    }
    assert_eq!(
        named.len(),
        6,
        "the A2A mapping no longer names all six anchors; what it lost: {:?}",
        [
            "context_id",
            "message_id",
            "AgentCard",
            "InputRequired",
            "kind",
            "parts"
        ]
        .iter()
        .filter(|t| !named.contains(*t))
        .collect::<Vec<_>>()
    );
}
