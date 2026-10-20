//! Every integration test for this crate, as one binary: one link and one
//! process per crate rather than one per file.
#![allow(clippy::print_stderr)]

mod dsh_descriptor;
mod zstd_source;
