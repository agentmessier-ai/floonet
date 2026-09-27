//! Every integration test for `fl`, as one binary.
//!
//! Cargo links each top-level `tests/*.rs` into its own crate against the whole
//! dependency graph, and linking is where this workspace's build time goes.
//! `tests/it/main.rs` is one test target; a plain `tests/it.rs` would be a crate
//! root whose `mod foo` resolves against `tests/`, where cargo still compiles
//! every file as its own target. The cost is `--test <file>` granularity:
//! select a module with `cargo test -p fl <module>`, which filters by name.
#![allow(clippy::print_stderr)]

mod a2a_vocabulary;
mod address_honesty_e2e;
mod ambiguous_sender_e2e;
mod fld_shutdown_e2e;
mod install_script_guards;
mod open_envelope_e2e;
mod peer_listen_e2e;
mod pi_tool_parity;
mod plugin_manifest_guards;
mod reply_hint_e2e;
mod rollback_e2e;
mod turns_window_e2e;
