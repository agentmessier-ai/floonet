pub mod address;
pub mod id;
pub mod logging;
pub mod retrieval;
pub mod turn;
pub mod version;

pub use address::Addressability;
pub use id::SessionId;
pub use retrieval::{
    Capabilities, Coverage, Hit, Query, RawHit, RetrievalProvider, Retrieved, Scope, SessionRow,
    TurnCursor, TurnRef, DEFAULT_WINDOW,
};
pub use turn::{NormalizedTurn, ParseChunk, Role, SessionMeta, TitleSource, ToolCallDigest};
pub use version::{compare_builds, BuildMatch, BUILD_DATE, GIT_SHA, VERSION, VERSION_LINE};

/// Every run of non-alphanumeric characters collapsed to a single `-`,
/// lowercased.
///
/// Runtimes encode a session's cwd into its transcript directory name by
/// replacing the separators, so a real path never matches the raw string;
/// normalizing both sides is what lets one find the other. Defined once
/// because the `--folder` filter and the process-to-transcript lookup must
/// share one definition of "same folder".
pub fn folder_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> Millis {
    Millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    )
}

/// A wall-clock instant, milliseconds since the Unix epoch.
///
/// A distinct type from [`Secs`] because the failure is silent: `i64` accepts
/// either, and a unit mix-up renders as a plausible date without a panic or
/// an `Err`. `#[serde(transparent)]` keeps the wire format a bare JSON
/// integer, so an older peer is unaffected.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Millis(i64);

/// A wall-clock instant, seconds since the Unix epoch.
///
/// Only `schema_migration.applied_at` is stored this way: it is written in raw
/// SQL by the migration machinery before any of this loads, so converting it
/// would mean a migration mutating its own bookkeeping. Any Rust reader or
/// writer of that column takes the unit from this signature.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Secs(i64);

impl Millis {
    pub const fn new(v: i64) -> Self {
        Self(v)
    }
    /// Unwrap for a SQL parameter or an arithmetic step. Named rather than
    /// `Deref`: the boundary should be visible in a diff.
    pub const fn get(self) -> i64 {
        self.0
    }
    /// Truncating, and toward negative infinity so instants before 1970 do not
    /// round the wrong way. Explicit rather than a `From` impl — a unit change
    /// is exactly the step this type exists to make visible, and `.into()` is
    /// not visible.
    pub const fn to_secs_floor(self) -> Secs {
        Secs(self.0.div_euclid(1000))
    }

    /// An instant `ms` milliseconds earlier, saturating at `i64::MIN`. The
    /// `_ms` suffix marks the argument as a duration, distinct from the
    /// instant it is subtracted from.
    pub const fn saturating_sub_ms(self, ms: i64) -> Self {
        Self(self.0.saturating_sub(ms))
    }
}

/// The gap between two instants, in milliseconds.
///
/// A duration, not an instant, so it is a plain `i64`: returning `Millis`
/// would let `now - then` be stored as a point in time, the confusion this
/// type exists to prevent.
impl std::ops::Sub for Millis {
    type Output = i64;
    fn sub(self, rhs: Self) -> i64 {
        self.0 - rhs.0
    }
}

impl Secs {
    pub const fn new(v: i64) -> Self {
        Self(v)
    }
    pub const fn get(self) -> i64 {
        self.0
    }
    pub const fn to_millis(self) -> Millis {
        Millis(self.0 * 1000)
    }
}

/// Exit quietly when the reader of our stdout goes away.
///
/// Rust's runtime sets SIGPIPE to SIG_IGN before `main`, so a write to a
/// closed pipe returns EPIPE and `println!` turns that into a panic: `| head`
/// would end every command with a backtrace. Restoring SIG_DFL hands the
/// decision to the kernel, as `git` and `rg` do, instead of auditing every
/// print site. Called from `main` in each binary; it affects only this
/// process, so nothing the daemon spawns inherits it.
///
/// # Safety
/// `signal` with `SIG_DFL` on `SIGPIPE` touches no memory and cannot fail in a
/// way that matters here; the return value is the previous handler, discarded.
pub fn exit_quietly_on_broken_pipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
