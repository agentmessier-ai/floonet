//! Every integration test for this crate, as one binary: one link per crate
//! rather than one per file.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod fanout_test;
mod local_api;
mod pairing_test;
mod search_limits;
mod server_test;
mod signed_uri_roundtrip;
