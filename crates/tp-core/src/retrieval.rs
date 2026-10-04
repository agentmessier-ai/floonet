//! The retrieval seam. Both the scan backend and the index backend
//! satisfy `RetrievalProvider`, and the *only* way to build a public `Hit` is
//! through `Hit::redacted_from`, so no backend can emit unscrubbed content.

use crate::id::SessionId;
use crate::turn::Role;
use std::time::Duration;

pub const DEFAULT_WINDOW: Duration = Duration::from_secs(6 * 3600);

/// What a caller is allowed to ask for. `since` always has a value — an
/// unbounded query must be requested explicitly by widening it, never by
/// omission.
#[derive(Debug, Clone)]
pub struct Scope {
    /// Match against session cwd — name, basename, or substring. `None` = every folder.
    pub folder: Option<String>,
    pub since: Duration,
    /// Empty = all known runtimes.
    pub runtimes: Vec<String>,
    /// Exclusive upper bound, unix ms. `None` = up to now. `since` alone only
    /// expresses "the last N" with a window ending now; a specific past day
    /// needs both ends.
    pub until: Option<i64>,
}

impl Scope {
    /// `(lower, upper)` in unix ms — the window as both providers must read it.
    pub fn range_ms(&self, now_ms: crate::Millis) -> (i64, Option<i64>) {
        (
            now_ms.get().saturating_sub(self.since.as_millis() as i64),
            self.until,
        )
    }

    /// The folder filter as every provider must see it: trimmed, without a
    /// trailing separator. Defined once so `--folder /a/b/` and `--folder /a/b`
    /// cannot disagree depending on which backend answers — an empty list
    /// reads as "you never worked there", not "the filter misread you".
    pub fn folder_needle(&self) -> Option<String> {
        let f = self.folder.as_ref()?;
        let t = f.trim().trim_end_matches(['/', '\\']);
        (!t.is_empty()).then(|| t.to_string())
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            folder: None,
            since: DEFAULT_WINDOW,
            runtimes: Vec::new(),
            until: None,
        }
    }
}

impl Scope {
    /// True when this scope forces a provider without `unscoped_ok` into its
    /// slow path — the CLI/MCP layer warns rather than silently blocking.
    pub fn is_broad(&self) -> bool {
        self.folder.is_none() && self.since > Duration::from_secs(24 * 3600)
    }
}

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub regex: bool,
    /// Off by default at every layer: `thinking` is opt-in to search *and* to
    /// return.
    pub include_thinking: bool,
    pub limit: usize,
}

/// Not telemetry — a correctness contract. A caller concluding "I never
/// discussed X" from a truncated scan is the failure mode designed against.
/// It carries only what changes a reader's next move: a running counter would
/// make the two fields that matter read as boilerplate.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub truncated: bool,
    /// Set when the provider could not fully honour the request (e.g. the index
    /// is behind the transcripts, a peer timed out).
    pub degraded: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Retrieved<T> {
    pub items: Vec<T>,
    pub coverage: Coverage,
}

impl<T> Retrieved<T> {
    pub fn new(items: Vec<T>, coverage: Coverage) -> Self {
        Self { items, coverage }
    }
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Retrieved<U> {
        Retrieved {
            items: self.items.into_iter().map(f).collect(),
            coverage: self.coverage,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    /// Whether an unscoped query is cheap. False for scan.
    pub unscoped_ok: bool,
    /// Whether `Query::regex` is honored. The FTS5 index matches tokenized
    /// phrases and cannot evaluate a regex; it would silently degrade to a
    /// literal search and report a false "no matches". Declared so the caller
    /// can refuse instead.
    pub regex: bool,
    /// Whether this provider's corpus can be incomplete in a way it can name.
    ///
    /// A provider reading transcript files off disk answers only for the files
    /// that are still there, and a runtime deletes them on its own schedule, so
    /// "no matches" and "that window is partly gone" are different answers. A
    /// provider holding its own copy has no such gap. Declared rather than
    /// inferred from the provider's name: branching on a name is the thing
    /// descriptors exist to stop, and the second provider to be written would
    /// not know a string comparison was waiting for it.
    pub reports_unscannable: bool,
}

/// The turn's coordinate, and what each part is for.
///
/// `uuid` — the source record's own id — is the address: unique within a
/// session even when `ts` is not, since parallel tool results and compaction
/// replays share a `(session_id, ts)`. `seq` is the storage key
/// (`UNIQUE(session_id, seq)`) and the cheap ordinal for order and pagination.
/// `ts` is an attribute for time windows and sort order, not an identity.
/// `ts` and `uuid` are `Option` because store-family runtimes may expose
/// neither; such a turn degrades to `seq` ordering — degraded, never wrong.
#[derive(Debug, Clone)]
pub struct TurnRef {
    pub session_id: String,
    pub ts: Option<i64>,
    pub seq: Option<i64>,
    pub uuid: Option<String>,
}

/// Provider-internal, pre-redaction. Deliberately not re-exported for public use.
#[derive(Debug, Clone)]
pub struct RawHit {
    pub at: TurnRef,
    pub machine_id: String,
    pub cwd: Option<String>,
    pub role: Role,
    pub excerpt: String,
    /// A subagent said this, not the operator (`Provenance::sidechain`).
    pub sidechain: bool,
    /// Whether the matched turn is still live context. "This evidence was
    /// compacted out of context" changes what the caller does with a hit, so
    /// it travels with the hit rather than costing a second read.
    pub surface: crate::turn::Surface,
}

/// The public search result. Constructible only via `redacted_from`.
#[derive(Debug, Clone)]
pub struct Hit {
    pub at: TurnRef,
    pub machine_id: String,
    pub cwd: Option<String>,
    pub role: Role,
    excerpt: String,
    pub sidechain: bool,
    pub surface: crate::turn::Surface,
}

impl Hit {
    /// The single funnel every backend's output passes through.
    /// `scrub` is injected rather than imported so `tp-core` stays dependency-free.
    pub fn redacted_from(raw: RawHit, scrub: &dyn Fn(&str) -> String) -> Self {
        Self {
            excerpt: scrub(&raw.excerpt),
            at: raw.at,
            machine_id: raw.machine_id,
            cwd: raw.cwd,
            role: raw.role,
            sidechain: raw.sidechain,
            surface: raw.surface,
        }
    }

    pub fn excerpt(&self) -> &str {
        &self.excerpt
    }
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub runtime_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub last_turn_at: Option<i64>,
    /// `None` from a scan provider — counting turns means parsing the whole file.
    pub turn_count: Option<i64>,
}

/// Where to read from in a session. Time-based so both backends agree.
#[derive(Debug, Clone, Copy)]
pub enum TurnCursor {
    Start,
    AfterTs(i64),
    /// A time window, kept newest-first when it doesn't fit.
    ///
    /// `AfterTs` pages forward from a point and on overflow keeps the oldest
    /// turns — right for replaying a session from the beginning. "What
    /// happened in the last few hours" starts at now, wants the newest turns
    /// kept on overflow, and pages backwards through `before_ms`.
    Window {
        /// Inclusive lower bound.
        since_ms: i64,
        /// Exclusive upper bound; `None` = now. Paging backwards means passing
        /// the earliest `ts` of the previous page, and exclusivity is what keeps
        /// that turn from being returned twice.
        before_ms: Option<i64>,
    },
}

impl TurnCursor {
    /// Whether a turn's timestamp falls in this cursor's range. One definition
    /// for both providers, so the scan side's line filter and the index side's
    /// row filter cannot drift.
    pub fn admits_ts(&self, ts: Option<i64>) -> bool {
        match (self, ts) {
            (TurnCursor::Start, _) => true,
            // A turn with no parseable timestamp can't be placed in a window, and
            // guessing would put it in every page. Only the unbounded read keeps it.
            (_, None) => false,
            (TurnCursor::AfterTs(cut), Some(ts)) => ts > *cut,
            (
                TurnCursor::Window {
                    since_ms,
                    before_ms,
                },
                Some(ts),
            ) => ts >= *since_ms && before_ms.is_none_or(|b| ts < b),
        }
    }
}

/// Accumulates a windowed read, dropping the oldest turns when it overflows.
///
/// The counterpart to `admit_turn`: the two providers must cut in exactly the
/// same place, so the rule lives here. Memory stays bounded — turns are evicted
/// as they go over, so a long window over a huge session never materializes.
pub struct WindowBuffer {
    out: std::collections::VecDeque<crate::turn::NormalizedTurn>,
    used: usize,
    limit: usize,
    budget: usize,
    dropped: bool,
}

impl WindowBuffer {
    pub fn new(limit: usize, budget: usize) -> Self {
        Self {
            out: std::collections::VecDeque::new(),
            used: 0,
            limit,
            budget,
            dropped: false,
        }
    }

    pub fn push(&mut self, mut turn: crate::turn::NormalizedTurn) {
        truncate_field(&mut turn.text, "… [turn truncated]");
        truncate_field(&mut turn.thinking, "… [thinking truncated]");
        self.used += turn.text.len() + turn.thinking.len();
        self.out.push_back(turn);
        // `len() > 1` for the same reason `admit_turn` admits an oversized first
        // turn: returning nothing would read as "this window is empty".
        while (self.out.len() > self.limit || self.used > self.budget) && self.out.len() > 1 {
            if let Some(old) = self.out.pop_front() {
                self.used -= old.text.len() + old.thinking.len();
                self.dropped = true;
            }
        }
    }

    /// `(turns oldest-first, dropped anything)`.
    pub fn finish(self) -> (Vec<crate::turn::NormalizedTurn>, bool) {
        (self.out.into(), self.dropped)
    }
}

/// Byte ceiling for one `turns` call. `limit` bounds the number of turns, which
/// bounds nothing that matters: a turn's size is whatever the other session
/// wrote. ~40 KB ≈ 10k tokens — enough to carry a working conversation over,
/// bounded enough that one call cannot evict the caller's context.
pub const DEFAULT_TURN_BUDGET_BYTES: usize = 40_000;

/// Per-turn ceiling, so a single enormous message can't consume the whole
/// budget by itself and starve the turns around it — which is what makes a
/// conversation readable rather than one wall of text.
pub const MAX_TURN_BYTES: usize = 4_000;

fn truncate_field(s: &mut String, marker: &str) {
    if s.len() <= MAX_TURN_BYTES {
        return;
    }
    let mut cut = MAX_TURN_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(marker);
}

/// Trim a turn to `MAX_TURN_BYTES` per field and charge it against `used`.
/// Returns `false` when the budget is spent and the caller must stop reading.
/// Lives here, not in either provider, so scan and index agree on where they
/// cut by construction rather than by discipline.
pub fn admit_turn(
    out: &mut Vec<crate::turn::NormalizedTurn>,
    mut turn: crate::turn::NormalizedTurn,
    used: &mut usize,
    budget: usize,
) -> bool {
    truncate_field(&mut turn.text, "… [turn truncated]");
    truncate_field(&mut turn.thinking, "… [thinking truncated]");
    let cost = turn.text.len() + turn.thinking.len();
    // An oversized first turn is admitted: it is already capped, and an empty
    // result would read as "this session is empty".
    if !out.is_empty() && *used + cost > budget {
        return false;
    }
    *used += cost;
    out.push(turn);
    true
}

pub trait RetrievalProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn search(&self, q: &Query, scope: &Scope) -> anyhow::Result<Retrieved<RawHit>>;
    fn sessions(&self, scope: &Scope, limit: usize) -> anyhow::Result<Retrieved<SessionRow>>;
    /// Stops at `limit` turns OR `budget_bytes`, whichever comes first, and
    /// reports which via `Coverage::truncated`. A truncated read is resumable:
    /// the caller passes the last returned turn's `ts` back as
    /// `TurnCursor::AfterTs`.
    fn turns(
        &self,
        session: &SessionId,
        at: TurnCursor,
        include_thinking: bool,
        limit: usize,
        budget_bytes: usize,
    ) -> anyhow::Result<Retrieved<crate::turn::NormalizedTurn>>;
}
