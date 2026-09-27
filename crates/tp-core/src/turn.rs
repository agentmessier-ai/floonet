//! The adapter contract: every runtime format is normalized into these
//! shapes before it ever touches storage. Adapters produce `ParseChunk`s;
//! `tp-db::writer` consumes them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Only `{name, input_digest}` survives into storage — full tool inputs are
/// large and frequently carry secrets. Retrieving the full input
/// is a separate, explicitly-opted-in read straight from the source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDigest {
    pub name: String,
    /// Short digest of the input (e.g. first N bytes / a hash), never the raw payload.
    pub input_digest: Option<String>,
}

/// Identity, lineage and cost that the source record carries and floonet keeps,
/// grouped so adding another such field never touches every place a
/// `NormalizedTurn` is constructed. Captured at index time because source
/// transcripts age out; what is not stored before then is unrecoverable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The source record's own id — the sound turn coordinate, where the format
    /// has one (JSONL family). `None` for store-family runtimes without a stable
    /// per-message id, which fall back to `seq` ordering.
    pub uuid: Option<String>,
    /// Edge to the previous turn in the source DAG. Kept even though we serve a
    /// flat list: a graph can always be flattened, never rebuilt after the fact.
    pub parent_uuid: Option<String>,
    /// This turn was written by a subagent, not by the operator's own thread —
    /// Claude Code's `isSidechain`. Kept per-turn even though the marker is in
    /// practice a property of the session (subagent files carry it on every
    /// record): per-turn is what the record states, and a session-level
    /// summary can be derived from it but not the reverse.
    ///
    /// A bool rather than a three-state, unlike `surface`: for runtimes with
    /// no such marker, "no record says so" and "the record says false" lead a
    /// caller filtering subagent noise to the same action — include the turn.
    #[serde(default)]
    pub sidechain: bool,
    /// The model that produced this turn. Changes mid-session (opus↔haiku,
    /// subagents), so it belongs on the turn, not the session.
    pub model: Option<String>,
    /// Prompt-cache accounting. Without these, a cached session's cost reads
    /// ~10× too high from `tokens_in` alone.
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
}

/// Whether a turn is still part of its runtime's live context.
///
/// Three states, and merging any two changes what a caller does: `Current` is
/// live context, `Superseded` is history a compaction removed, and `Unknown`
/// means floonet could not tell. Reading `Unknown` as `Current` asserts that
/// superseded content is still live, which `tracks_compaction` exists to prevent.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Current,
    Superseded,
    /// The default everywhere a value is missing: an adapter at parse time
    /// (surface is decided by whoever sees the whole session, not per record),
    /// a wire peer too old to send it, a row written before the column existed.
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedTurn {
    pub role: Role,
    /// Unix ms; adapters best-effort this, may be `None` for malformed records.
    pub ts: Option<i64>,
    pub text: String,
    /// Empty string, not `Option`, so `turn_fts` triggers never special-case NULL.
    pub thinking: String,
    /// Reasoning happened but cannot be read — an encrypted or safety-redacted
    /// payload, with no text for `thinking`. A bool rather than a three-state
    /// enum because the other states are derivable from `thinking`: empty means
    /// none, non-empty means text. Defaulted, so adapters without it are unchanged.
    #[serde(default)]
    pub thinking_opaque: bool,
    pub tool_calls: Vec<ToolCallDigest>,
    /// Filled by the read side, not by adapters: one record cannot know whether
    /// a later compaction superseded it. The writer decides via SQL back-updates
    /// as boundaries arrive; the scan provider via `apply_compaction` over the
    /// whole parsed file. The two must agree turn by turn.
    #[serde(default)]
    pub surface: Surface,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// Source identity/lineage/cost. Populated by adapters that have it; `Default`
    /// (all `None`) otherwise. Grouped so the construction sites don't churn.
    pub prov: Provenance,
}

/// Where a session title came from. Kept apart because a caller acts on them
/// differently: one shared `title` slot cannot distinguish "this runtime has
/// no title" from "floonet did not look". Precedence is `User > Ai > Derived`,
/// the same resolution Codex applies when importing Claude Code sessions.
/// Resolution happens in SQL on read (`COALESCE(title_user, title_ai,
/// title_derived)`) so a title arriving later wins without a rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSource {
    /// A person named this session: Claude Code's `custom-title` (`/rename`),
    /// pi's `session_info.name`, Codex's `threads.name` (`/name`).
    User,
    /// A model named it.
    Ai,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub cwd: Option<String>,
    /// Set by a person, in the runtime. Outranks everything.
    pub title_user: Option<String>,
    /// Generated by a model, in the runtime.
    pub title_ai: Option<String>,
    /// floonet's own fallback — the first user message, truncated. A last
    /// resort, and marked as one: every runtime surveyed has a native title, so
    /// this being the only value present means floonet did not find theirs.
    pub title_derived: Option<String>,
    pub started_at: Option<i64>,
}

impl SessionMeta {
    /// Record a title a runtime stated. The last non-empty statement wins
    /// within a source, matching the runtimes' own rules. An empty statement
    /// records nothing: in a chunk a `None` is indistinguishable from "no title
    /// here", so the writer's `COALESCE(new, old)` cannot propagate a clear,
    /// and honouring it here would promise an erasure the database never performs.
    pub fn set_title(&mut self, source: TitleSource, title: impl Into<String>) {
        let title = title.into();
        if title.trim().is_empty() {
            return;
        }
        match source {
            TitleSource::User => self.title_user = Some(title),
            TitleSource::Ai => self.title_ai = Some(title),
        }
    }

    /// Whether any runtime-stated title was found. `false` means the derived
    /// fallback is all there is — a fact worth being able to ask about.
    pub fn has_native_title(&self) -> bool {
        self.title_user.is_some() || self.title_ai.is_some()
    }
}

/// Where a compaction cut the conversation.
///
/// Two shapes because two runtimes differ, and collapsing them errs in the
/// worst direction — reporting live context as superseded. Claude Code's
/// marker is positional: everything before it is gone from context. pi's
/// `compaction` entry names `firstKeptEntryId`, which points earlier than the
/// marker itself by a varying amount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionBoundary {
    /// Everything at or before this index in `turns` is superseded. The index
    /// counts turns seen BEFORE the marker, so a marker that is itself a turn
    /// (pi's summary) lands after the boundary and stays current.
    At(usize),
    /// Everything before the turn whose source id is this is superseded — and
    /// that turn itself is kept. Resolved by the writer against `turn.uuid`,
    /// which means it does nothing when the anchor is not in the index; marking
    /// everything on a failed lookup would be the opposite of conservative.
    Before(String),
}

/// The scan provider's half of the surface question, over a fully parsed
/// session. The other half is `tp-db::writer`, which applies the same
/// boundaries incrementally in SQL as chunks arrive and cannot share this
/// code; the two are held to turn-by-turn equality by test.
///
/// Semantics, matching the writer exactly:
/// - `At(n)`: the first `n` turns are superseded (`n` counts turns seen before
///   the marker; writer: `seq <= prev_last_seq + n`).
/// - `Before(uuid)`: turns before the one carrying that uuid are superseded and
///   the anchor itself is kept (writer: `seq < (SELECT seq … WHERE uuid = ?)`).
///   An anchor that resolves to nothing marks nothing: a positional guess
///   would err toward calling live context dead.
/// - `tracks` false: every turn is `Unknown`, never `Current` — a runtime
///   floonet cannot read compaction for has not said its history is live.
pub fn apply_compaction(
    turns: &mut [NormalizedTurn],
    boundaries: &[CompactionBoundary],
    tracks: bool,
) {
    if !tracks {
        for t in turns.iter_mut() {
            t.surface = Surface::Unknown;
        }
        return;
    }
    for t in turns.iter_mut() {
        t.surface = Surface::Current;
    }
    for b in boundaries {
        let cut = match b {
            CompactionBoundary::At(n) => *n,
            CompactionBoundary::Before(uuid) => {
                match turns
                    .iter()
                    .position(|t| t.prov.uuid.as_deref() == Some(uuid.as_str()))
                {
                    Some(j) => j,
                    None => continue,
                }
            }
        };
        for t in turns.iter_mut().take(cut) {
            t.surface = Surface::Superseded;
        }
    }
}

/// Result of `Adapter::parse_from`. `new_offset` MUST NOT include a torn final
/// line — an adapter that includes an unterminated line's bytes in the offset
/// will silently drop that turn's data on the next poll.
#[derive(Debug, Clone, Default)]
pub struct ParseChunk {
    pub turns: Vec<NormalizedTurn>,
    pub new_offset: u64,
    pub meta: SessionMeta,
    /// Compaction boundaries seen in this chunk: what stopped being context.
    ///
    /// Reported rather than applied, because the turns a boundary supersedes were
    /// written by an EARLIER chunk and are already in the database. Only the
    /// writer knows their `seq`.
    pub compaction: Vec<CompactionBoundary>,
    /// Whether this adapter can recognise its runtime's compaction marker at all.
    ///
    /// Not the same as `compaction` being empty: a session with no compaction
    /// and a runtime floonet cannot read compaction for both produce an empty
    /// vec, and calling both `current` would assert that superseded content is
    /// still context. Turns from an adapter that answers `false` stay `unknown`.
    pub tracks_compaction: bool,
}

#[cfg(test)]
mod apply_compaction_tests {
    use super::*;

    fn t(uuid: &str) -> NormalizedTurn {
        NormalizedTurn {
            role: Role::User,
            ts: None,
            text: uuid.into(),
            thinking: String::new(),
            thinking_opaque: false,
            tool_calls: vec![],
            surface: Surface::Unknown,
            tokens_in: None,
            tokens_out: None,
            prov: Provenance {
                uuid: Some(uuid.into()),
                ..Default::default()
            },
        }
    }

    fn surfaces(turns: &[NormalizedTurn]) -> Vec<Surface> {
        turns.iter().map(|t| t.surface).collect()
    }

    /// The writer computes the same answer incrementally in SQL and cannot
    /// share this code; these pin the in-memory half in isolation.
    #[test]
    fn a_positional_boundary_supersedes_exactly_the_turns_before_it() {
        let mut turns = vec![t("a"), t("b"), t("c"), t("d")];
        // At(2): 2 turns were seen before the marker — writer: seq <= prev + 2.
        apply_compaction(&mut turns, &[CompactionBoundary::At(2)], true);
        use Surface::*;
        assert_eq!(surfaces(&turns), [Superseded, Superseded, Current, Current]);
    }

    #[test]
    fn an_anchored_boundary_keeps_the_turn_it_names() {
        let mut turns = vec![t("a"), t("b"), t("keep"), t("d")];
        apply_compaction(
            &mut turns,
            &[CompactionBoundary::Before("keep".into())],
            true,
        );
        use Surface::*;
        assert_eq!(surfaces(&turns), [Superseded, Superseded, Current, Current]);
    }

    #[test]
    fn an_unresolvable_anchor_marks_nothing() {
        let mut turns = vec![t("a"), t("b")];
        apply_compaction(
            &mut turns,
            &[CompactionBoundary::Before("absent".into())],
            true,
        );
        assert_eq!(surfaces(&turns), [Surface::Current, Surface::Current]);
    }

    #[test]
    fn two_compactions_accumulate_rather_than_fight() {
        let mut turns = vec![t("a"), t("b"), t("c"), t("d")];
        apply_compaction(
            &mut turns,
            &[CompactionBoundary::At(3), CompactionBoundary::At(1)],
            true,
        );
        use Surface::*;
        // The wider cut wins regardless of order — superseded is never unset.
        assert_eq!(
            surfaces(&turns),
            [Superseded, Superseded, Superseded, Current]
        );
    }

    #[test]
    fn an_adapter_that_cannot_track_compaction_answers_unknown_not_current() {
        let mut turns = vec![t("a"), t("b")];
        apply_compaction(&mut turns, &[], false);
        assert_eq!(surfaces(&turns), [Surface::Unknown, Surface::Unknown]);
    }
}
