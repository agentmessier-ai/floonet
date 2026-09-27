//! Declarative adapter: one parser engine, configured per runtime, so adding
//! a runtime is a descriptor file rather than Rust and a release.
//!
//! Everything that differs between runtimes is field mapping — where the role,
//! blocks, usage and native id live. A branchable transcript tree needs no
//! special handling because the retrieval coordinate is `(session_id, ts)`,
//! never a tree position. What is not configurable lives once, shared: line
//! splitting with torn-write tolerance, `SessionMeta` extraction, and the
//! `locate`/`discover` walk. A format the vocabulary cannot express still
//! drops to a Rust `Adapter` impl.
//!
//! `config` holds the descriptor vocabulary, the embedded shipped descriptors
//! and loading; `engine` holds the parser they drive. Both are re-exported so
//! `decl::` paths name everything.

mod config;
mod engine;

pub use config::*;
pub use engine::*;

/// A descriptor from `install/runtimes.d/`, the file that ships, rather than a
/// config a test built: the engine supporting a rule and the shipped
/// descriptor carrying it are different facts, and tests must pin the second.
#[cfg(test)]
pub(crate) fn shipped_config(name: &str) -> DeclConfig {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../install/runtimes.d")
        .join(format!("{name}.toml"));
    toml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}
