//! Reading: which session, and what is in it.
//!
//! The one place the two surfaces are allowed to differ is what to do with an
//! ambiguous time-based read, and that difference is returned as data rather
//! than settled here: see `Resolution`.

use anyhow::{Context, Result};
use tp_core::retrieval::{Query, Scope, TurnCursor};
use tp_core::turn::NormalizedTurn;
use tp_core::{Coverage, Hit, Retrieved, SessionId, SessionRow};
use tp_search::Retrieval;

/// Which session a time-based read landed on.
///
/// Reading by time reads one session, which is a guess. The CLI refuses an
/// uncertain guess and lists the candidates; MCP takes the most recent and
/// says how many matched. Settling that is a product decision, so the
/// outcome is returned as data and each surface applies its own rule.
pub enum Resolution {
    /// Exactly one session matched the window.
    One(SessionRow),
    /// Several matched. `candidates` is ordered most-recent first, so the head
    /// is what a caller that chooses to guess should guess.
    Ambiguous(Vec<SessionRow>),
    /// Nothing was active in the window.
    None,
}

/// The most recently active session(s) in a window.
pub fn resolve_session(r: &Retrieval, scope: &Scope, limit: usize) -> Result<Resolution> {
    let found = r.sessions(scope, limit)?;
    Ok(match found.items.len() {
        0 => Resolution::None,
        1 => Resolution::One(found.items.into_iter().next().expect("len == 1")),
        _ => Resolution::Ambiguous(found.items),
    })
}

/// Turns from one session. `session_id` is parsed here rather than by the
/// caller so that a malformed id fails the same way on both surfaces.
pub fn turns(
    r: &Retrieval,
    session_id: &str,
    cursor: TurnCursor,
    include_thinking: bool,
    limit: usize,
    budget_bytes: Option<usize>,
) -> Result<Retrieved<NormalizedTurn>> {
    let sid = SessionId::parse(session_id).with_context(|| {
        format!("malformed session id {session_id:?} (want <machine>/<runtime>/<native>)")
    })?;
    r.turns(&sid, cursor, include_thinking, limit, budget_bytes)
}

/// Sessions active in a window.
pub fn sessions(r: &Retrieval, scope: &Scope, limit: usize) -> Result<Retrieved<SessionRow>> {
    r.sessions(scope, limit)
}

/// Search this machine.
pub fn search(r: &Retrieval, q: &Query, scope: &Scope) -> Result<Retrieved<Hit>> {
    r.search(q, scope)
}

/// Why a search that matched nothing might have matched nothing.
///
/// The provider matches the query as a single phrase, so a multi-word query
/// with no hits reads as "this was never discussed" when it means "those words
/// never appeared in that order": a fact about the query rendered as a fact
/// about the corpus, unless the output says so.
///
/// `None` when there were hits, when the query is a single word (the phrasing
/// cannot be what excluded anything), or under `--regex`, where the caller has
/// already said how matching works.
pub fn empty_note(q: &Query, hits: usize) -> Option<String> {
    if hits > 0 || q.regex || !q.text.trim().contains(char::is_whitespace) {
        return None;
    }
    let words: Vec<&str> = q.text.split_whitespace().collect();
    Some(format!(
        "{:?} was searched as ONE PHRASE — those {} words in that order, not as \
         separate terms. Try a single distinctive word, or --regex to say what \
         you mean.",
        q.text,
        words.len()
    ))
}

/// What this window holds that a scan can never read, however long it runs.
///
/// The scan provider answers from transcript files, and runtimes delete those
/// after a retention period. Sessions the index knows about whose file is gone
/// are invisible to a scan, which reports `no matches` and blames its file
/// budget: the same failure `empty_note` exists for, a fact about the provider
/// rendered as a fact about the corpus.
///
/// `None` when the provider cannot have this gap, and when the window holds no
/// such session. Costs one `stat` per session in the window.
///
/// `pub(crate)` because `&tp_db::Db` is an argument: `App::unscannable_note` is
/// the way in, so the storage handle stays inside this crate.
pub(crate) fn unscannable_note(
    reports_unscannable: bool,
    scope: &Scope,
    db: &tp_db::Db,
) -> Option<String> {
    if !reports_unscannable {
        return None;
    }
    let (since_ms, until_ms) = scope.range_ms(tp_core::now_ms());
    // A failure to compute the note is not the absence of one. This function
    // exists so that sessions the scan cannot read are never counted as absent
    // from the corpus, and answering `None` on a read error would do exactly
    // that one layer up.
    let claimed = match tp_db::query::sessions_claiming_a_file(db.conn(), since_ms, until_ms) {
        Ok(c) => c,
        Err(e) => {
            return Some(format!(
                "could not check which sessions still have a transcript on disk, so this \
                 answer's completeness is unknown: {e}"
            ))
        }
    };
    let (mut sessions, mut turns) = (0usize, 0i64);
    for (path, n) in claimed {
        if !std::path::Path::new(&path).exists() {
            sessions += 1;
            turns += n;
        }
    }
    // Sessions that never had a file (push-ingested runtimes), which the query
    // above cannot see. Counted into the same number: the causes differ but
    // the caller's action does not.
    match tp_db::query::sessions_without_a_file(db.conn(), since_ms, until_ms) {
        Ok((n, t)) => {
            sessions += n;
            turns += t;
        }
        // Same reason as above: skipping this half on an error would under-count
        // and then report the shortfall as a clean corpus.
        Err(e) => {
            return Some(format!(
                "could not count sessions whose runtime keeps no transcript, so this \
                 answer's completeness is unknown: {e}"
            ))
        }
    }
    if sessions == 0 {
        return None;
    }
    Some(format!(
        // The one place a user meets this trade-off, so it is stated here:
        // floonet reads transcripts and keeps no copy of its own, and rows
        // left from before that decision are unreachable by any command.
        // Saying so is the difference between a known limit and apparent
        // data loss.
        "{sessions} session(s) in this window ({turns} turns) have no transcript on disk — \
         a scan CANNOT read them, and they are missing from this answer. floonet keeps no \
         copy of its own, so nothing else will show them either. These are rows left from \
         an older version that did index turns; if the transcript was deleted, that content \
         is not recoverable through floonet."
    ))
}

/// Whether a result was cut short. A truncated read that does not say so
/// reads as a complete one, so the decision has one home even though the
/// rendering does not.
pub fn is_partial(c: &Coverage) -> bool {
    c.truncated || c.degraded.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolution reports ambiguity instead of resolving it, and orders the
    /// candidates so a caller that chooses to guess guesses the newest.
    #[test]
    fn ambiguity_is_returned_not_decided() {
        let rows = vec![
            SessionRow {
                id: "m/rt/new".into(),
                runtime_id: "rt".into(),
                cwd: None,
                title: None,
                last_turn_at: Some(200),
                turn_count: None,
            },
            SessionRow {
                id: "m/rt/old".into(),
                runtime_id: "rt".into(),
                cwd: None,
                title: None,
                last_turn_at: Some(100),
                turn_count: None,
            },
        ];
        match Resolution::Ambiguous(rows) {
            Resolution::Ambiguous(c) => {
                assert_eq!(c.len(), 2);
                assert_eq!(
                    c[0].id, "m/rt/new",
                    "most recent first — a caller that guesses must guess the newest"
                );
            }
            _ => panic!("expected Ambiguous"),
        }
    }

    fn q(text: &str, regex: bool) -> Query {
        Query {
            text: text.to_string(),
            regex,
            include_thinking: false,
            limit: 20,
        }
    }

    /// The note fires exactly when the phrasing could be what excluded
    /// everything: a note on every empty result would be ignored as noise.
    #[test]
    fn the_phrase_note_fires_only_when_phrasing_could_be_the_reason() {
        let note = empty_note(&q("refutation pass defects dismissed", false), 0)
            .expect("a multi-word query with no hits must explain the phrase rule");
        assert!(note.contains("ONE PHRASE"), "{note}");
        assert!(note.contains('4'), "should name how many words: {note}");

        assert!(
            empty_note(&q("refutation pass", false), 3).is_none(),
            "hits mean the phrasing excluded nothing"
        );
        assert!(
            empty_note(&q("refutation", false), 0).is_none(),
            "a single word cannot have been split"
        );
        assert!(
            empty_note(&q("a b c", true), 0).is_none(),
            "under --regex the caller already said how matching works"
        );
        assert!(
            empty_note(&q("  spaced  ", false), 0).is_none(),
            "surrounding whitespace is not two words"
        );
    }

    #[test]
    fn partial_covers_both_truncation_and_degradation() {
        let none = Coverage::default();
        assert!(!is_partial(&none));
        assert!(is_partial(&Coverage {
            truncated: true,
            ..Default::default()
        }));
        assert!(is_partial(&Coverage {
            degraded: Some("a peer timed out".into()),
            ..Default::default()
        }));
    }
}
