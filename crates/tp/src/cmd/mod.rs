//! The commands, grouped by the surface they serve. `main.rs` keeps the clap
//! definitions, the shared small helpers (paths, ids, time parsing) and
//! `main()` itself; each module here is one command family. Everything is
//! re-exported from the crate root so `mcp.rs` addresses them as `crate::`.

pub(crate) mod net;
pub(crate) mod reach;
pub(crate) mod read;
