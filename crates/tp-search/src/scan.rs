//! On-demand scan provider, the shipping default. Three filter layers:
//!   1. mtime window prunes the file set before anything is opened
//!   2. raw-substring test on the un-parsed line (no JSON parse)
//!   3. `Adapter::parse_line` only on lines that survived (2)
//!
//! Layer 2 is what avoids a full JSON parse of the corpus. A regex query has
//! no cheap prefilter, so it parses every line in the window, bounded by
//! layer 1 and reported via `Coverage`.

use anyhow::Result;
use regex::Regex;
use std::path::PathBuf;
use tp_core::retrieval::{
    Capabilities, Coverage, Query, RawHit, RetrievalProvider, Retrieved, Scope, SessionRow,
    TurnCursor, TurnRef,
};
use tp_core::turn::NormalizedTurn;
use tp_core::SessionId;
use tp_ingest::adapter::{Adapter, SourceFile};

const EXCERPT_RADIUS: usize = 60;

pub struct ScanProvider {
    machine_id: String,
    adapters: Vec<Box<dyn Adapter>>,
    roots: Vec<(String, PathBuf)>,
}

impl ScanProvider {
    pub fn new(
        machine_id: impl Into<String>,
        adapters: Vec<Box<dyn Adapter>>,
        roots: Vec<(String, PathBuf)>,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            adapters,
            roots,
        }
    }

    fn adapter_for(&self, runtime_id: &str) -> Option<&dyn Adapter> {
        self.adapters
            .iter()
            .find(|a| a.id() == runtime_id)
            .map(|a| a.as_ref())
    }

    /// Runtimes in scope whose transcript root is not on this machine.
    ///
    /// "No sessions" and "no directory" are different answers, and only the
    /// first is about the corpus: a runtime not installed here, or whose home
    /// was moved by its environment variable, yields an empty result either
    /// way, so the result has to say which. Reported, never fatal: one absent
    /// root among several present is a partial answer, not a failed one.
    fn absent_roots(&self, scope: &Scope) -> Vec<String> {
        self.roots
            .iter()
            .filter(|(id, root)| {
                (scope.runtimes.is_empty() || scope.runtimes.iter().any(|r| r == id))
                    && self.adapter_for(id).is_some()
                    && !root.exists()
            })
            .map(|(id, root)| format!("{id} ({})", root.display()))
            .collect()
    }

    /// Layer 1: prune by mtime, runtime and folder before opening anything.
    ///
    /// Only the lower bound can prune here: mtime is the last write, so a file
    /// older than `since` holds no turn in the window, but a session active
    /// both inside and after the window carries a recent mtime, and an upper
    /// bound on it would drop the file being asked for. The upper bound is
    /// applied per turn.
    fn candidates(&self, scope: &Scope) -> Result<Vec<(String, SourceFile)>> {
        let since_ms = tp_core::now_ms().saturating_sub_ms(scope.since.as_millis() as i64);
        let folder = scope.folder_needle();
        let mut out = Vec::new();
        for (runtime_id, root) in &self.roots {
            if !scope.runtimes.is_empty() && !scope.runtimes.iter().any(|r| r == runtime_id) {
                continue;
            }
            let Some(adapter) = self.adapter_for(runtime_id) else {
                continue;
            };
            for src in adapter.discover(root)? {
                if src.mtime_ms < since_ms.get() {
                    continue;
                }
                if let Some(folder) = &folder {
                    if !path_matches_folder(&src.path, folder) {
                        continue;
                    }
                }
                out.push((runtime_id.clone(), src));
            }
        }
        // Most-recent first: with a limit, the useful hits come back first.
        out.sort_by_key(|(_, src)| std::cmp::Reverse(src.mtime_ms));
        Ok(out)
    }
}

/// Fold "these runtimes are not installed here" into an existing note.
/// Appended rather than replacing: an absent root is additional context and
/// must not displace a coverage warning already in the note.
fn note_absent(degraded: Option<String>, absent: &[String]) -> Option<String> {
    if absent.is_empty() {
        return degraded;
    }
    let note = format!(
        "not searched — no transcript directory on this machine for: {}. \
         That is not 'no sessions': the runtime is either not installed here, \
         or its home was moved by the environment variable its descriptor names.",
        absent.join(", ")
    );
    Some(match degraded {
        Some(d) => format!("{d} {note}"),
        None => note,
    })
}

/// Fold "these transcripts are in a format this build cannot read" into a note.
fn note_refused(degraded: Option<String>, refused: &[String]) -> Option<String> {
    if refused.is_empty() {
        return degraded;
    }
    let note = format!(
        "some transcripts were SKIPPED, not searched: {}. Upgrade floonet, or \
         its descriptor, before reading this answer as complete.",
        refused.join("; ")
    );
    Some(match degraded {
        Some(d) => format!("{d} {note}"),
        None => note,
    })
}

/// Folder match: case-insensitive substring, against the transcript path both
/// literally and separator-normalized.
///
/// This prunes on the file path, before any file is opened, but the path is
/// the encoded form of the cwd while `sessions` displays the real one. The
/// slug comparison lets a displayed cwd be pasted back as the filter; a
/// literal-only match would answer "no sessions" for it.
fn path_matches_folder(path: &std::path::Path, folder: &str) -> bool {
    let hay = path.to_string_lossy().to_lowercase();
    let needle = folder.trim().to_lowercase();
    // The literal test widens the slug test; it never narrows it.
    hay.contains(&needle) || tp_core::folder_slug(&hay).contains(&tp_core::folder_slug(&needle))
}

/// Layer-2 pre-filter: does this raw line hold `needle_lower` anywhere,
/// case-insensitively? The caller lowercases the needle once; it is invariant
/// across every line of every file.
///
/// A false negative here drops a real hit, so the general path is
/// `to_lowercase`. The byte path is taken only when both sides are ASCII,
/// where `to_lowercase` is exactly ASCII case-folding; a non-ASCII line can
/// lowercase into ASCII (U+212A, U+0130), so a byte scan of it would miss.
fn line_contains_ci(line: &str, needle_lower: &str) -> bool {
    let nb = needle_lower.as_bytes();
    if nb.is_empty() {
        return true;
    }
    if needle_lower.is_ascii() && line.is_ascii() {
        let (lo, up) = (nb[0], nb[0].to_ascii_uppercase());
        return line
            .as_bytes()
            .windows(nb.len())
            .any(|w| (w[0] == lo || w[0] == up) && w.eq_ignore_ascii_case(nb));
    }
    line.to_lowercase().contains(needle_lower)
}

/// Case-insensitive substring search returning a byte offset into `hay`.
///
/// `hay.to_lowercase().find(..)` cannot serve: `to_lowercase` does not
/// preserve byte length (U+212A shrinks, U+0130 grows), so its index can land
/// mid-codepoint in the original and panic on slicing.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle_lower = needle.to_lowercase();
    // Upper bound, in characters, on how much of `hay` the needle can span:
    // lowercasing never merges characters. One extra because `to_lowercase` is
    // context-sensitive (final-sigma), so the character after the span must be
    // present for the fold to match a whole-string fold.
    let span = needle_lower.chars().count() + 1;

    // Walk real char boundaries of `hay`, lowercasing only a needle-length span
    // at each, so the offset stays anchored to `hay`. Lowercasing the whole
    // suffix at every position would be quadratic in the field length, and
    // fields are uncapped.
    hay.char_indices()
        .find(|(i, _)| {
            let rest = &hay[*i..];
            let end = rest
                .char_indices()
                .nth(span)
                .map_or(rest.len(), |(byte, _)| byte);
            rest[..end].to_lowercase().starts_with(&needle_lower)
        })
        .map(|(i, _)| i)
}

fn excerpt_around(text: &str, query: &str, re: Option<&Regex>) -> String {
    let found = match re {
        Some(r) => r.find(text).map(|m| m.start()),
        None => find_ci(text, query),
    };
    let Some(idx) = found else {
        return text.chars().take(EXCERPT_RADIUS * 2).collect();
    };
    // `idx` is a real boundary of `text`, so these slices are safe.
    let start = text[..idx]
        .char_indices()
        .rev()
        .nth(EXCERPT_RADIUS)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = text[idx..]
        .char_indices()
        .nth(EXCERPT_RADIUS * 2)
        .map(|(i, _)| idx + i)
        .unwrap_or(text.len());
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(text[start..end].trim());
    if end < text.len() {
        s.push('…');
    }
    s
}

fn matches(hay: &str, query: &str, re: Option<&Regex>) -> bool {
    match re {
        Some(r) => r.is_match(hay),
        // Same predicate as `find_ci`, so "matched" and "can locate it for the
        // excerpt" never disagree.
        None => find_ci(hay, query).is_some(),
    }
}

impl RetrievalProvider for ScanProvider {
    fn name(&self) -> &'static str {
        "scan"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unscoped_ok: false, // an unscoped scan opens the whole corpus; warn the caller
            regex: true,        // evaluated per-line against the real text
            // Reads transcripts where they lie, so a deleted file is a hole
            // this provider can describe but not fill.
            reports_unscannable: true,
        }
    }

    fn search(&self, q: &Query, scope: &Scope) -> Result<Retrieved<RawHit>> {
        let re = if q.regex {
            Some(Regex::new(&q.text)?)
        } else {
            None
        };
        // Hoisted: the layer-2 pre-filter runs on every line of every file.
        let needle_lower = q.text.to_lowercase();
        let absent = self.absent_roots(scope);
        let candidates = self.candidates(scope)?;
        let window = scope.range_ms(tp_core::now_ms());
        let mut coverage = Coverage::default();

        // Every candidate is opened. A file-count cap would drop the oldest
        // mtimes, which are the finished sessions, and a query's window says
        // nothing about when a file was last written.
        let mut hits = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        'outer: for (runtime_id, src) in candidates.iter() {
            let Some(adapter) = self.adapter_for(runtime_id) else {
                continue;
            };
            let Ok(content) = tp_ingest::source::read_text(&src.path) else {
                continue;
            };
            // A format this build was not written against is refused, not
            // parsed: a descriptor's paths describe one version and under-read
            // another silently. Skipping and saying so cannot be mistaken for
            // an empty session.
            if let Some(why) = content
                .lines()
                .find(|l| !l.is_empty())
                .and_then(|l| adapter.unsupported_reason(l))
            {
                if !refused.contains(&why) {
                    refused.push(why);
                }
                continue;
            }

            let session_id =
                SessionId::new(&self.machine_id, runtime_id, &src.native_id).to_string();
            // `cwd` is probed on the first non-empty record, before the
            // prefilter: a runtime may state it once, at the head, and a head
            // that does not hold the needle must not leave the file's hits
            // without a directory. The loop below still probes survivors.
            let mut cwd: Option<String> = content
                .lines()
                .find(|l| !l.is_empty())
                .and_then(|l| adapter.cwd_of_line(l));
            // Hits from this file, held back until the whole file has been
            // seen: their `surface` cannot be known any earlier.
            let mut file_hits: Vec<(RawHit, Option<String>)> = Vec::new();

            for line in content.lines() {
                if line.is_empty() {
                    continue;
                }
                // Layer 2, substring queries only: a raw line without the
                // needle has no parsed field holding it either.
                if re.is_none() && !line_contains_ci(line, &needle_lower) {
                    continue;
                }
                if cwd.is_none() {
                    cwd = adapter.cwd_of_line(line);
                }
                // Layer 3: parse only survivors, through the adapter so every
                // reader agrees on format quirks.
                let Some(turn) = adapter.parse_line(line) else {
                    continue;
                };
                // Per-turn window: the mtime prune is per file, and a
                // long-lived session would otherwise contribute hits from
                // outside the window.
                if !in_window(turn.ts, window) {
                    continue;
                }

                let mut hit_field: Option<String> = None;
                if matches(&turn.text, &q.text, re.as_ref()) {
                    hit_field = Some(turn.text.clone());
                } else if q.include_thinking && matches(&turn.thinking, &q.text, re.as_ref()) {
                    hit_field = Some(turn.thinking.clone());
                } else if let Some(tc) = turn
                    .tool_calls
                    .iter()
                    .find(|t| matches(&t.name, &q.text, re.as_ref()))
                {
                    hit_field = Some(format!("[tool] {}", tc.name));
                }

                // Scrub the whole field before cutting the excerpt: rules
                // anchored on a closing delimiter (the `"` of an access token,
                // the `-----END` of a private key) no-op once the window cuts
                // the closer off. `Retrieval` scrubs again downstream; this is
                // the pass that sees the full text.
                let hit_field = hit_field.map(|f| tp_ingest::redact::scrub(&f));

                if let Some(field) = hit_field {
                    file_hits.push((
                        RawHit {
                            at: TurnRef {
                                session_id: session_id.clone(),
                                ts: turn.ts,
                                seq: None,
                                uuid: turn.prov.uuid.clone(),
                            },
                            machine_id: self.machine_id.clone(),
                            cwd: cwd.clone(),
                            role: turn.role,
                            excerpt: excerpt_around(&field, &q.text, re.as_ref()),
                            sidechain: turn.prov.sidechain,
                            // Resolved below, once the whole file has been seen.
                            surface: tp_core::turn::Surface::Unknown,
                        },
                        turn.prov.uuid.clone(),
                    ));
                    if hits.len() + file_hits.len() >= q.limit {
                        coverage.truncated = true;
                        break;
                    }
                }
            }

            // Resolve `surface`, only for files that produced hits. The line
            // loop cannot answer it: a compaction marker is not a turn and
            // flows past `parse_line`, and a marker near the end supersedes
            // turns matched near the start. So hit-bearing files get the
            // whole-file parse and `apply_compaction` the turns path uses,
            // and each hit is matched back by uuid, falling back to timestamp
            // for runtimes without one. At most `q.limit` files pay this.
            if !file_hits.is_empty() {
                if let Ok(mut chunk) = adapter.parse_from(&src.path, 0) {
                    tp_core::turn::apply_compaction(
                        &mut chunk.turns,
                        &chunk.compaction,
                        chunk.tracks_compaction,
                    );
                    for (hit, uuid) in &mut file_hits {
                        let found = match uuid {
                            Some(u) => chunk
                                .turns
                                .iter()
                                .find(|t| t.prov.uuid.as_deref() == Some(u.as_str())),
                            None => chunk.turns.iter().find(|t| t.ts == hit.at.ts),
                        };
                        if let Some(t) = found {
                            hit.surface = t.surface;
                        }
                        // Unmatched stays Unknown, never promoted to Current.
                    }
                }
                hits.extend(file_hits.into_iter().map(|(h, _)| h));
            }
            if coverage.truncated {
                break 'outer;
            }
        }
        coverage.degraded = note_refused(note_absent(coverage.degraded, &absent), &refused);
        Ok(Retrieved::new(hits, coverage))
    }

    fn sessions(&self, scope: &Scope, limit: usize) -> Result<Retrieved<SessionRow>> {
        let absent = self.absent_roots(scope);
        let candidates = self.candidates(scope)?;
        let window = scope.range_ms(tp_core::now_ms());
        let mut coverage = Coverage::default();
        let mut rows = Vec::new();
        let mut examined = 0usize;
        for (runtime_id, src) in candidates.iter() {
            if rows.len() >= limit {
                break;
            }
            let Some(adapter) = self.adapter_for(runtime_id) else {
                continue;
            };
            examined += 1;

            // "Active in the window" is decided by turn timestamps, not mtime:
            // mtime is the file's last write, which can be seconds newer than
            // its last turn. Affordable because the loop stops at `limit` and
            // candidates arrive mtime-ordered, so on the order of `limit`
            // files are parsed.
            let Some(last_turn_at) = last_ts_in_window(adapter, &src.path, window) else {
                continue; // no turns in the window: not active for this query
            };
            let last_turn_at = Some(last_turn_at);

            // Read only enough to recover cwd + title; never the whole file.
            let (cwd, title) = head_meta(adapter, &src.path);
            rows.push(SessionRow {
                id: SessionId::new(&self.machine_id, runtime_id, &src.native_id).to_string(),
                runtime_id: runtime_id.clone(),
                cwd,
                title,
                last_turn_at,
                turn_count: None, // counting means parsing the whole file
            });
        }
        // Most-recently-active first is the contract; candidates were sorted
        // by mtime, which is not the same key.
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_turn_at));
        coverage.truncated = candidates.len() > examined;
        coverage.degraded = note_absent(coverage.degraded, &absent);
        Ok(Retrieved::new(rows, coverage))
    }

    fn turns(
        &self,
        session: &SessionId,
        at: TurnCursor,
        include_thinking: bool,
        limit: usize,
        budget_bytes: usize,
    ) -> Result<Retrieved<NormalizedTurn>> {
        let empty = || Retrieved {
            items: Vec::new(),
            coverage: Coverage {
                truncated: false,
                degraded: None,
            },
        };
        // "I looked and there is nothing" and "I cannot look here" are the
        // same empty vector and must not be the same answer: these returns
        // carry `degraded` so a provider limitation is never reported as an
        // empty corpus. `Capabilities` has no flag for "cannot see that
        // runtime", so the note is the only signal.
        let unreadable = |why: String| Retrieved {
            items: Vec::new(),
            coverage: Coverage {
                truncated: false,
                degraded: Some(why),
            },
        };
        let Some(adapter) = self.adapter_for(&session.runtime_id) else {
            return Ok(unreadable(format!(
                // Attributes the emptiness to floonet, not the session, and
                // names no fallback route: there is none.
                "runtime '{}' has no descriptor on this machine, so a scan \
                 cannot read it — this is NOT an empty session, and floonet \
                 cannot tell you what is in it by any route. Add a descriptor \
                 under install/runtimes.d to make it readable.",
                session.runtime_id
            )));
        };
        let Some((_, root)) = self.roots.iter().find(|(r, _)| *r == session.runtime_id) else {
            return Ok(unreadable(format!(
                // Nothing writes turns into floonet by any other path, so a
                // runtime with no transcript on disk is simply unreadable.
                "runtime '{}' has no transcript root on this machine, so there \
                 is nothing for a scan to read. This answer is empty for a \
                 reason that has nothing to do with the session, and no other \
                 route will show it: floonet reads transcripts and keeps no \
                 copy of its own.",
                session.runtime_id
            )));
        };
        // `locate`, not `discover().find()`: reading one session must not cost
        // an enumeration of every session on the machine.
        let Some(src) = adapter.locate(root, &session.native_id)? else {
            return Ok(empty());
        };
        // `parse_from`, not a per-line `parse_line` loop: a compaction marker
        // is not a turn and flows past `parse_line`, so only a whole-file
        // parse yields the boundary list and `tracks_compaction` that
        // `apply_compaction` needs to answer `surface`.
        let mut chunk = adapter.parse_from(&src.path, 0)?;
        // Thinking is opt-in at read time. Dropped here rather than at each
        // push site: the parse has to read the field anyway, so this is where
        // "parsed" becomes "returned".
        if !include_thinking {
            for t in &mut chunk.turns {
                t.thinking.clear();
            }
        }
        tp_core::turn::apply_compaction(
            &mut chunk.turns,
            &chunk.compaction,
            chunk.tracks_compaction,
        );
        // A windowed read keeps the newest turns, so it cannot stop early. A
        // forward read keeps the oldest and stops as soon as the budget is spent.
        let (out, truncated) = if let TurnCursor::Window { .. } = at {
            let mut buf = tp_core::retrieval::WindowBuffer::new(limit, budget_bytes);
            for turn in chunk.turns {
                if !at.admits_ts(turn.ts) {
                    continue;
                }
                buf.push(turn);
            }
            buf.finish()
        } else {
            let mut out = Vec::new();
            let mut used = 0usize;
            let mut truncated = false;
            // Reaching the limit does not mean something was left behind: a
            // session with exactly `limit` turns is complete. One turn past
            // the limit is admitted as the probe, then truncated below.
            for turn in chunk.turns {
                if !at.admits_ts(turn.ts) {
                    continue;
                }
                // Budget first: `admit_turn` caps the turn and reports whether
                // it fit.
                if !tp_core::retrieval::admit_turn(&mut out, turn, &mut used, budget_bytes) {
                    truncated = true;
                    break;
                }
                // `>` is the +1 probe; `max(1)` keeps `limit = 0` meaning "at
                // least one turn", never an empty result that reads as an
                // empty session.
                if out.len() > limit.max(1) {
                    truncated = true;
                    out.truncate(limit.max(1));
                    break;
                }
            }
            (out, truncated)
        };
        Ok(Retrieved {
            items: out,
            coverage: Coverage {
                truncated,
                degraded: None,
            },
        })
    }
}

/// `true` when `ts` falls inside `(since, until)`; `until` is exclusive.
///
/// A turn with no timestamp cannot be placed in a window. It is kept only
/// when there is no upper bound, where the file-level mtime prune already
/// stands in for `since`.
fn in_window(ts: Option<i64>, (since, until): (i64, Option<i64>)) -> bool {
    match ts {
        Some(ts) => ts >= since && until.is_none_or(|u| ts < u),
        None => until.is_none(),
    }
}

/// The latest turn timestamp inside the window, or `None` if the session has
/// no turns there. Costs a full parse of the file.
fn last_ts_in_window(
    adapter: &dyn Adapter,
    path: &std::path::Path,
    window: (i64, Option<i64>),
) -> Option<i64> {
    let content = tp_ingest::source::read_text(path).ok()?;
    content
        .lines()
        .filter_map(|l| adapter.parse_line(l))
        .filter(|t| in_window(t.ts, window))
        .filter_map(|t| t.ts)
        .max()
}

/// `(cwd, title)` for a transcript. Two kinds of title exist: one the runtime
/// states as its own entry (`adapter::title_of_line`), and the fallback, the
/// first user message truncated. The stated one wins and is looked for over
/// the whole file, since a rename can land anywhere; cwd and the fallback are
/// bounded to the head.
fn head_meta(adapter: &dyn Adapter, path: &std::path::Path) -> (Option<String>, Option<String>) {
    let Ok(content) = tp_ingest::source::read_text(path) else {
        return (None, None);
    };
    let mut cwd = None;
    let mut derived: Option<String> = None;
    let mut stated: Option<(tp_core::turn::TitleSource, String)> = None;

    for (i, line) in content.lines().enumerate() {
        // A stated title may be appended at any point; it is the only reason
        // to keep reading past the head.
        if let Some((source, t)) = adapter.title_of_line(line) {
            let better = match (&stated, source) {
                // A person's title outranks a model's, whatever order they
                // were appended in.
                (Some((tp_core::turn::TitleSource::User, _)), tp_core::turn::TitleSource::Ai) => {
                    false
                }
                // Otherwise the last statement wins: a rename replaces itself.
                _ => true,
            };
            if better && !t.trim().is_empty() {
                stated = Some((source, tp_ingest::redact::scrub(&t)));
            }
        }

        if i >= HEAD_LINES {
            continue;
        }
        if cwd.is_none() {
            cwd = adapter.cwd_of_line(line);
        }
        if derived.is_none() {
            if let Some(t) = adapter.parse_line(line) {
                if matches!(t.role, tp_core::turn::Role::User) && !t.text.is_empty() {
                    // Scrub before truncating: same delimiter-anchored rule
                    // hazard as the search excerpt.
                    derived = Some(
                        tp_ingest::redact::scrub(&t.text)
                            .chars()
                            .take(tp_ingest::adapter::jsonl::TITLE_CHARS)
                            .collect(),
                    );
                }
            }
        }
    }

    (cwd, stated.map(|(_, t)| t).or(derived))
}

/// How far into a file the cwd and the fallback title are looked for. Both
/// come from the opening of a session, so they are either in the head or absent.
const HEAD_LINES: usize = 200;

#[cfg(test)]
mod find_ci_tests {
    use super::find_ci;

    /// The span-bounded search must agree exactly with the naive whole-suffix
    /// definition: the caller slices `hay` at the returned offset, and case
    /// folding can change a character's byte length.
    #[test]
    fn find_ci_agrees_with_the_naive_definition() {
        fn naive(hay: &str, needle: &str) -> Option<usize> {
            if needle.is_empty() {
                return Some(0);
            }
            let n = needle.to_lowercase();
            hay.char_indices()
                .find(|(i, _)| hay[*i..].to_lowercase().starts_with(&n))
                .map(|(i, _)| i)
        }
        for (hay, needle) in [
            ("hello world", "WORLD"),
            ("HELLO world", "hello"),
            ("", "x"),
            ("abc", ""),
            ("abc", "abcd"),
            ("aaa", "aa"),
            ("→ → → floonet ←", "TELEPORT"),
            ("STRASSE", "strasse"),
            ("İstanbul", "istanbul"),
            ("no match here", "zzq"),
            ("émile ÉMILE", "ÉMILE"),
            ("ΣΟΦΟΣ", "σοφος"),
        ] {
            assert_eq!(
                find_ci(hay, needle),
                naive(hay, needle),
                "find_ci disagreed on ({hay:?}, {needle:?})"
            );
            if let Some(i) = find_ci(hay, needle) {
                assert!(hay.is_char_boundary(i), "offset {i} is not a char boundary");
            }
        }
    }

    /// Growing the input must not grow the work quadratically. The bound is
    /// generous: it catches a quadratic shape, not a number for one machine.
    #[test]
    fn a_non_matching_field_does_not_cost_quadratic_time() {
        let make = |kb: usize| -> String { "build log line noise ".repeat(kb * 1024 / 21) };
        let time = |s: &str| {
            let t = std::time::Instant::now();
            assert_eq!(find_ci(s, "zzq-no-such-token"), None);
            t.elapsed()
        };
        let small = time(&make(50));
        let big = time(&make(200)); // 4x the input
        assert!(
            big < small * 20,
            "4x input cost {big:?} vs {small:?} — that is the quadratic shape returning"
        );
    }
}
