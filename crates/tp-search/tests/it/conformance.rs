//! Provider conformance: one body of assertions, run against every provider.
//! An abstraction exercised through one implementation is not a seam.
//!
//! Any divergence must be either fixed or declared in `Capabilities`; those
//! are the only two legal outcomes, so the tests assert on capabilities
//! wherever providers legitimately differ.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tp_core::retrieval::{Query, Scope, TurnCursor};
use tp_core::SessionId;
use tp_search::{Retrieval, ScanProvider};

const MACHINE: &str = "m-test";
const SESSION_UUID: &str = "11111111-2222-3333-4444-555555555555";

/// A fake `~/.claude/projects` tree with one session holding a secret in
/// `thinking`, the case the redaction funnel must catch on every backend.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let ts = |off: i64| {
        let dt = now_ms - off;
        chrono_fmt(dt)
    };

    let lines = [
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": ts(5000),
            "message": {"content": "how do we handle pagination"}
        }),
        serde_json::json!({
            "type": "assistant", "timestamp": ts(4000),
            "message": {"content": [
                {"type": "thinking", "thinking": "the token is sk-ant-oat01-SECRETSECRETSECRETSECRET and pagination uses cursors"},
                {"type": "text", "text": "use a cursor-based approach"},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}
            ], "usage": {"input_tokens": 10, "output_tokens": 20}}
        }),
        // Non-conversational record: both backends must skip it identically.
        serde_json::json!({"type": "queue-operation", "timestamp": ts(3000)}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(proj.join(format!("{SESSION_UUID}.jsonl")), body).unwrap();
    (dir, root)
}

fn chrono_fmt(ms: i64) -> String {
    // RFC3339, which the adapter parses.
    let secs = ms / 1000;
    let nsec = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsec)
        .unwrap()
        .to_rfc3339()
}

fn scan_provider(root: &Path) -> Retrieval {
    Retrieval::new(Box::new(ScanProvider::new(
        MACHINE,
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )))
}

fn wide_scope() -> Scope {
    Scope {
        folder: None,
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    }
}

fn q(text: &str, include_thinking: bool) -> Query {
    Query {
        text: text.to_string(),
        regex: false,
        include_thinking,
        limit: 50,
    }
}

// ── The conformance body: identical assertions, run per provider ──────────

fn assert_finds_text(r: &Retrieval) {
    let got = r.search(&q("pagination", false), &wide_scope()).unwrap();
    assert!(
        !got.items.is_empty(),
        "[{}] must find 'pagination' in text",
        r.provider_name()
    );
    assert!(
        got.items
            .iter()
            .all(|h| h.at.session_id.ends_with(SESSION_UUID)),
        "[{}] hits must carry the composite session id",
        r.provider_name()
    );
    assert!(
        got.items.iter().all(|h| h.at.ts.is_some()),
        "[{}] every hit must carry the universal (session_id, ts) coordinate",
        r.provider_name()
    );
}

fn assert_thinking_gate(r: &Retrieval) {
    let name = r.provider_name();
    // "cursors" appears only inside thinking.
    let hidden = r.search(&q("cursors", false), &wide_scope()).unwrap();
    assert!(
        hidden.items.is_empty(),
        "[{name}] thinking must not be searched when include_thinking=false"
    );

    // Searching thinking when opted in is required of every provider, not a
    // capability.
    let shown = r.search(&q("cursors", true), &wide_scope()).unwrap();
    assert!(
        !shown.items.is_empty(),
        "[{name}] thinking must be searchable when opted in"
    );
}

/// The security-critical invariant: no backend may emit an unredacted secret,
/// regardless of whether it scrubbed at write time or read time.
fn assert_redaction_on_every_path(r: &Retrieval) {
    let name = r.provider_name();
    const SECRET: &str = "sk-ant-oat01-SECRETSECRETSECRETSECRET";

    let hits = r.search(&q("pagination", true), &wide_scope()).unwrap();
    for h in &hits.items {
        assert!(
            !h.excerpt().contains(SECRET),
            "[{name}] search excerpt leaked a secret"
        );
    }

    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let turns = r
        .turns(&sid, TurnCursor::Start, true, 50, None)
        .unwrap()
        .items;
    assert!(
        !turns.is_empty(),
        "[{name}] turns() must return the session's turns"
    );
    for t in &turns {
        assert!(
            !t.text.contains(SECRET),
            "[{name}] turn text leaked a secret"
        );
        assert!(
            !t.thinking.contains(SECRET),
            "[{name}] turn thinking leaked a secret"
        );
    }
    assert!(
        turns
            .iter()
            .any(|t| t.thinking.contains("[redacted:anthropic-key]")),
        "[{name}] the secret must be replaced by a visible placeholder, not silently dropped"
    );
}

fn assert_sessions_listed(r: &Retrieval) {
    let got = r.sessions(&wide_scope(), 10).unwrap();
    assert_eq!(
        got.items.len(),
        1,
        "[{}] must list exactly the one fixture session",
        r.provider_name()
    );
    let s = &got.items[0];
    assert!(s.id.ends_with(SESSION_UUID));
    assert_eq!(
        s.cwd.as_deref(),
        Some("/Users/test/dev/demo"),
        "[{}] cwd",
        r.provider_name()
    );
}

fn assert_turn_cursor(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let all = r
        .turns(&sid, TurnCursor::Start, false, 50, None)
        .unwrap()
        .items;
    assert_eq!(
        all.len(),
        2,
        "[{name}] exactly 2 conversational turns (queue-operation must be skipped)"
    );

    let first_ts = all[0].ts.expect("ts present");
    let rest = r
        .turns(&sid, TurnCursor::AfterTs(first_ts), false, 50, None)
        .unwrap()
        .items;
    assert_eq!(
        rest.len(),
        1,
        "[{name}] AfterTs must exclude turns at or before the cursor"
    );
}

/// The byte budget must cut at the same place on every backend, and say so.
/// `limit` bounds turn count, not bytes: a turn is as big as whatever was
/// written, so an unbudgeted read can evict the caller's context. A truncated
/// read that looks complete is the same failure as a truncated search
/// reported as exhaustive.
fn assert_turn_byte_budget(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);

    // A starved budget still yields exactly one turn: the first is admitted
    // (per-turn capped) so an oversized head cannot make a session read as
    // empty.
    let tiny = r
        .turns(&sid, TurnCursor::Start, false, 50, Some(1))
        .unwrap();
    assert_eq!(
        tiny.items.len(),
        1,
        "[{name}] a starved budget must still return the first turn, not nothing"
    );
    assert!(
        tiny.coverage.truncated,
        "[{name}] stopping early MUST be reported as truncated"
    );
    assert!(
        tiny.items[0].ts.is_some(),
        "[{name}] a truncated read must carry a ts to resume from"
    );

    // A generous budget returns everything and does not claim truncation.
    let full = r
        .turns(&sid, TurnCursor::Start, false, 50, Some(10_000_000))
        .unwrap();
    assert_eq!(full.items.len(), 2, "[{name}] full read returns both turns");
    assert!(
        !full.coverage.truncated,
        "[{name}] a complete read must not be reported as truncated"
    );

    // Resuming from the truncated read reaches what was cut off: the budget
    // delays turns, it never drops them.
    let resumed = r
        .turns(
            &sid,
            TurnCursor::AfterTs(tiny.items[0].ts.unwrap()),
            false,
            50,
            Some(10_000_000),
        )
        .unwrap();
    assert_eq!(
        resumed.items.len(),
        1,
        "[{name}] resuming after a truncated read must return the remainder"
    );
}

/// Punctuation in a query must not error or behave specially on one backend
/// only: a phrase-quoting backend tokenizes `claude/settings` to the same
/// adjacent pair a substring backend matches literally.
fn assert_punctuated_query_parity(r: &Retrieval) {
    let name = r.provider_name();
    let scope = Scope {
        folder: None,
        since: std::time::Duration::from_secs(86_400 * 3650),
        runtimes: vec![],
        until: None,
    };
    for probe in ["claude/settings", "a-b", "x.y"] {
        let q = Query {
            text: probe.to_string(),
            regex: false,
            include_thinking: false,
            limit: 20,
        };
        // The corpus need not contain them; only that the query does not fail.
        let got = r.search(&q, &scope);
        assert!(
            got.is_ok(),
            "[{name}] a punctuated query must not fail: {probe:?}"
        );
    }
}

/// `--folder` must accept the cwd the tool itself prints, on both `sessions`
/// and `search`. The scan prunes on the transcript path, which is the encoded
/// cwd (`-Users-test-dev-demo`), so a real path only matches through
/// normalization; a silent empty result here reads as "you never worked there".
fn assert_folder_filter(r: &Retrieval) {
    let name = r.provider_name();
    let scoped = |folder: &str| Scope {
        folder: Some(folder.to_string()),
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    };

    // The exact string `sessions` displays for this fixture, and a fragment.
    for needle in ["/Users/test/dev/demo", "/Users/test/dev/demo/", "demo"] {
        let got = r.sessions(&scoped(needle), 10).unwrap();
        assert!(
            !got.items.is_empty(),
            "[{name}] --folder {needle:?} must find the session whose cwd IS that path"
        );
    }

    // Widening must not have made the filter meaningless.
    let none = r
        .sessions(&scoped("/Users/test/dev/unrelated-project"), 10)
        .unwrap();
    assert!(
        none.items.is_empty(),
        "[{name}] --folder must still exclude a non-matching folder"
    );

    // The same filter through `search`. The negative case is the load-bearing
    // one: an over-broad result reads as "I searched only there, and this is
    // all there is", a wrong answer in the shape of a right one.
    for needle in ["/Users/test/dev/demo", "demo"] {
        let got = r.search(&q("pagination", false), &scoped(needle)).unwrap();
        assert!(
            !got.items.is_empty(),
            "[{name}] search --folder {needle:?} must still find hits in that folder"
        );
    }
    let out_of_folder = r
        .search(
            &q("pagination", false),
            &scoped("/Users/test/dev/zzz-nonexistent"),
        )
        .unwrap();
    assert!(
        out_of_folder.items.is_empty(),
        "[{name}] search --folder for a folder with no sessions must return NOTHING, \
         got {} hit(s) — a non-existent folder returning the full unfiltered result set \
         reads as a complete answer to a question that was never asked",
        out_of_folder.items.len()
    );
}

/// A window read keeps the newest turns; `AfterTs` keeps the oldest. Every
/// provider must agree on that, and on `before_ms` being exclusive: paging
/// backwards passes the previous page's earliest `ts`, so an inclusive bound
/// would return that turn on every page.
fn assert_window_cursor(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let wide = TurnCursor::Window {
        since_ms: now - 3_600_000,
        before_ms: None,
    };

    let all = r.turns(&sid, TurnCursor::Start, false, 50, None).unwrap();
    let win = r.turns(&sid, wide, false, 50, None).unwrap();
    assert_eq!(
        win.items.len(),
        all.items.len(),
        "[{name}] a window covering the whole session must return all of it"
    );

    // Overflow: the window keeps the last turn, the forward read the first.
    let one_win = r.turns(&sid, wide, false, 1, None).unwrap();
    let one_fwd = r.turns(&sid, TurnCursor::Start, false, 1, None).unwrap();
    assert_eq!(one_win.items.len(), 1, "[{name}] window honours limit");
    assert_eq!(
        one_win.items[0].ts,
        all.items.last().unwrap().ts,
        "[{name}] an overflowing window must keep the NEWEST turn"
    );
    assert_eq!(
        one_fwd.items[0].ts,
        all.items.first().unwrap().ts,
        "[{name}] an overflowing forward read must keep the OLDEST turn"
    );
    assert!(
        one_win.coverage.truncated,
        "[{name}] dropping turns must be reported, never silent"
    );

    // `before_ms` is exclusive, which is what makes paging back terminate.
    let cut = all.items.last().unwrap().ts.unwrap();
    let paged = r
        .turns(
            &sid,
            TurnCursor::Window {
                since_ms: now - 3_600_000,
                before_ms: Some(cut),
            },
            false,
            50,
            None,
        )
        .unwrap();
    assert!(
        paged.items.iter().all(|t| t.ts.unwrap() < cut),
        "[{name}] before_ms must be exclusive"
    );
    assert_eq!(
        paged.items.len(),
        all.items.len() - 1,
        "[{name}] paging back must return everything before the cut, and only that"
    );
}

/// The fixture's assistant turn calls `Bash`, and the read must carry it: a
/// tool-only turn with its calls dropped renders as though nothing was in it.
fn assert_tool_calls_survive(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let turns = r
        .turns(&sid, TurnCursor::Start, false, 50, None)
        .unwrap()
        .items;
    assert!(
        turns
            .iter()
            .any(|t| t.tool_calls.iter().any(|c| c.name == "Bash")),
        "[{name}] a turn's tool calls must survive the read"
    );
}

/// `since`/`until` mean the same thing to every provider, for both `search`
/// and `sessions`. The fixture's turns are seconds old, so a window that
/// ended an hour ago must come back empty; the failure shape guarded against
/// is returning recent data for an old window.
fn assert_time_window(r: &Retrieval) {
    let name = r.provider_name();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let scoped = |since: Duration, until: Option<i64>| Scope {
        folder: None,
        since,
        runtimes: vec![],
        until,
    };

    // A window that ended an hour ago holds none of the fixture's turns.
    let past = scoped(Duration::from_secs(86_400), Some(now - 3_600_000));
    assert!(
        r.sessions(&past, 10).unwrap().items.is_empty(),
        "[{name}] sessions must honour `until` — a window that ended before the \
         session existed cannot contain it"
    );
    assert!(
        r.search(&q("pagination", false), &past)
            .unwrap()
            .items
            .is_empty(),
        "[{name}] search must honour `until`"
    );

    // The same window extended to now finds it again, so the emptiness above
    // was the bound, not a broken query.
    let live = scoped(Duration::from_secs(86_400), None);
    assert_eq!(
        r.sessions(&live, 10).unwrap().items.len(),
        1,
        "[{name}] the same window ending now must find the session"
    );
    assert!(
        !r.search(&q("pagination", false), &live)
            .unwrap()
            .items
            .is_empty(),
        "[{name}] the same window ending now must find the hit"
    );

    // Lower bound: a window that starts after the turns were written.
    let future = scoped(Duration::from_millis(1), None);
    assert!(
        r.sessions(&future, 10).unwrap().items.is_empty(),
        "[{name}] sessions must honour `since` — the index ignored it entirely"
    );
}

/// A degenerate `limit` must not be answered with "this window is empty":
/// zero means at least one turn, and `usize::MAX` means everything, on every
/// provider, including ones that map the limit into a narrower integer type.
fn assert_degenerate_limits(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);

    for (label, limit) in [("zero", 0usize), ("usize::MAX", usize::MAX)] {
        let got = r
            .turns(&sid, TurnCursor::Start, false, limit, None)
            .unwrap()
            .items;
        assert!(
            !got.is_empty(),
            "[{name}] limit {label}: returned nothing, which reads as an empty window"
        );
    }

    // And the useful half: an enormous limit means everything, not one.
    let all = r
        .turns(&sid, TurnCursor::Start, false, usize::MAX, None)
        .unwrap()
        .items;
    assert_eq!(
        all.len(),
        2,
        "[{name}] an enormous limit must return the whole session"
    );
}

fn run_conformance(r: &Retrieval) {
    assert_finds_text(r);
    assert_time_window(r);
    assert_window_cursor(r);
    assert_tool_calls_survive(r);
    assert_thinking_gate(r);
    assert_redaction_on_every_path(r);
    assert_sessions_listed(r);
    assert_folder_filter(r);
    assert_turn_byte_budget(r);
    assert_punctuated_query_parity(r);
    assert_turn_cursor(r);
    assert_degenerate_limits(r);
}

#[test]
fn scan_provider_conforms() {
    let (_tmp, root) = fixture();
    run_conformance(&scan_provider(&root));
}

/// Two turns sharing one `timestamp`, the shape parallel tool results and
/// compaction replays take. `ts` cannot tell them apart; the address is
/// `uuid`, and both turns must come back each carrying its own. A dedicated
/// corpus, because a second turn in the shared fixture would ripple into the
/// other assertions' counts.
#[test]
fn same_timestamp_turns_are_distinct_by_uuid() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // One timestamp, two uuids: two distinct turns, not a coordinate collision.
    let ts = chrono_fmt(now_ms - 4000);
    let lines = [
        serde_json::json!({
            "type": "assistant", "timestamp": ts,
            "uuid": "aaaaaaaa-0000-0000-0000-000000000001",
            "message": {"content": [{"type": "text", "text": "parallel alpha result"}]}
        }),
        serde_json::json!({
            "type": "assistant", "timestamp": ts,
            "uuid": "aaaaaaaa-0000-0000-0000-000000000002",
            "message": {"content": [{"type": "text", "text": "parallel beta result"}]}
        }),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(proj.join(format!("{SESSION_UUID}.jsonl")), body).unwrap();

    {
        let (name, r) = ("scan", scan_provider(&root));
        let got = r.search(&q("parallel", false), &wide_scope()).unwrap();
        assert_eq!(
            got.items.len(),
            2,
            "{name}: two distinct turns share the ts; dropping one reads as a corpus that never had it"
        );
        let ts0 = got.items[0].at.ts;
        assert!(ts0.is_some(), "{name}: the shared ts must survive");
        assert_eq!(
            got.items[0].at.ts, got.items[1].at.ts,
            "{name}: the premise is the SAME ts"
        );
        let uuids: std::collections::HashSet<&str> = got
            .items
            .iter()
            .map(|h| {
                h.at.uuid
                    .as_deref()
                    .unwrap_or_else(|| panic!("{name}: the address must be surfaced"))
            })
            .collect();
        assert_eq!(
            uuids,
            [
                "aaaaaaaa-0000-0000-0000-000000000001",
                "aaaaaaaa-0000-0000-0000-000000000002",
            ]
            .into_iter()
            .collect(),
            "{name}: same ts, different uuids — that is the whole point"
        );
    }
}

const LONG_UUID: &str = "99999999-8888-7777-6666-555555555555";

/// A session with more turns than one page, which the two-turn main fixture
/// cannot exercise.
fn long_fixture(n: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-long");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let body: String = (0..n)
        .map(|i| {
            // Oldest first, one second apart, so every turn has a distinct ts.
            let line = serde_json::json!({
                "type": "user", "cwd": "/Users/test/dev/long",
                "timestamp": chrono_fmt(now_ms - ((n - i) as i64) * 1000),
                "message": {"content": format!("turn {i}")}
            });
            format!("{line}\n")
        })
        .collect();
    std::fs::write(proj.join(format!("{LONG_UUID}.jsonl")), body).unwrap();
    (dir, root)
}

/// Page a long session with `AfterTs`, the documented cursor, and collect
/// everything.
fn page_all(r: &Retrieval, limit: usize) -> Vec<String> {
    let sid = SessionId::new(MACHINE, "claude_code", LONG_UUID);
    let mut out: Vec<String> = Vec::new();
    let mut cursor = TurnCursor::Start;
    for _ in 0..100 {
        let got = r.turns(&sid, cursor, false, limit, None).unwrap();
        if got.items.is_empty() {
            break;
        }
        let last_ts = got.items.last().and_then(|t| t.ts).expect("ts present");
        out.extend(got.items.into_iter().map(|t| t.text));
        if !got.coverage.truncated {
            break;
        }
        cursor = TurnCursor::AfterTs(last_ts);
    }
    out
}

/// Paging with `AfterTs` must reach every turn, in order, however far past
/// the start the cursor sits; a provider that pre-fetches a bounded prefix
/// and filters on `ts` runs out of rows and reports an empty page as complete.
#[test]
fn paging_with_after_ts_agrees_across_backends() {
    const N: usize = 50;
    let (_tmp, root) = long_fixture(N);

    let expected: Vec<String> = (0..N).map(|i| format!("turn {i}")).collect();

    let scan = page_all(&scan_provider(&root), 7);
    assert_eq!(
        scan.len(),
        N,
        "[scan] paging lost turns: {} of {N}",
        scan.len()
    );
    assert_eq!(scan, expected, "[scan] wrong turns or wrong order");
}

/// The other half: a page that ends exactly on the last turn must not claim
/// there is more, and one that does not must not claim completeness.
#[test]
fn after_ts_reports_completeness_honestly_on_both_backends() {
    const N: usize = 20;
    let (_tmp, root) = long_fixture(N);
    let sid = SessionId::new(MACHINE, "claude_code", LONG_UUID);

    {
        let r = scan_provider(&root);
        let name = r.provider_name();
        let first = r.turns(&sid, TurnCursor::Start, false, 5, None).unwrap();
        assert!(
            first.coverage.truncated,
            "[{name}] 5 of {N} turns is not the whole session"
        );

        let ts_of_15th = r
            .turns(&sid, TurnCursor::Start, false, 15, None)
            .unwrap()
            .items
            .last()
            .and_then(|t| t.ts)
            .expect("ts present");
        let tail = r
            .turns(&sid, TurnCursor::AfterTs(ts_of_15th), false, 100, None)
            .unwrap();
        assert_eq!(
            tail.items.len(),
            5,
            "[{name}] the tail after turn 15 is 5 turns"
        );
        assert!(
            !tail.coverage.truncated,
            "[{name}] the tail IS the rest — claiming more is a lie"
        );

        // A limit exactly equal to what remains: the value between the two
        // comfortable cases above. Admitting the Nth turn is not evidence of
        // an (N+1)th, so this must read as complete.
        let whole = r.turns(&sid, TurnCursor::Start, false, N, None).unwrap();
        assert_eq!(whole.items.len(), N, "[{name}] asked for all {N}");
        assert!(
            !whole.coverage.truncated,
            "[{name}] a limit that exactly covers the session is not a truncation"
        );
    }
}

/// `surface`, turn by turn, over real compaction markers. Pins the answers of
/// `tp_core::turn::apply_compaction` on the scan path directly.
///
/// Built on the shipped descriptors rather than the built-in adapters: the
/// built-ins carry no compaction rules (the descriptor supersedes them by
/// id), so a run through them would answer `unknown == unknown` everywhere.
mod surface_conformance {
    use super::*;
    use tp_core::turn::Surface;
    use tp_ingest::adapter::decl::{load_configs, DeclAdapter};

    fn shipped(id: &str) -> DeclAdapter {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d");
        DeclAdapter::new(
            load_configs(&dir)
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("shipped {id}.toml must load")),
        )
    }

    /// The turns one session reads as, through the scan provider over a
    /// shipped descriptor.
    fn scan_turns(runtime: &str, native_id: &str, root: &Path) -> Vec<tp_core::NormalizedTurn> {
        let scan = Retrieval::new(Box::new(ScanProvider::new(
            MACHINE,
            vec![Box::new(shipped(runtime))],
            vec![(runtime.to_string(), root.to_path_buf())],
        )));
        let sid = SessionId::new(MACHINE, runtime, native_id);
        scan.turns(&sid, TurnCursor::Start, false, 100, None)
            .unwrap()
            .items
    }

    fn surfaces(turns: &[tp_core::NormalizedTurn]) -> Vec<Surface> {
        turns.iter().map(|t| t.surface).collect()
    }

    #[test]
    fn a_positional_marker_supersedes_the_turns_before_it() {
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-x-p");
        std::fs::create_dir_all(&proj).unwrap();
        let msg = |t: &str, ts: &str| {
            format!(r#"{{"type":"user","timestamp":"{ts}","message":{{"content":"{t}"}}}}"#)
        };
        std::fs::write(
            proj.join("aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                msg("old one", "2026-08-04T10:00:00+00:00"),
                msg("old two", "2026-08-04T10:01:00+00:00"),
                r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-08-04T10:02:00+00:00"}"#,
                msg("kept", "2026-08-04T10:03:00+00:00"),
            ),
        )
        .unwrap();

        let scan = scan_turns(
            "claude_code",
            "aaaaaaaa-1111-2222-3333-444444444444",
            root.path(),
        );
        use Surface::*;
        assert_eq!(
            surfaces(&scan),
            [Superseded, Superseded, Current],
            "everything before the boundary is superseded: {scan:?}"
        );
    }

    /// A search hit carries the same `surface` and `sidechain` a turn read
    /// does. The scan cannot know `surface` while walking lines (a marker near
    /// the end supersedes a match near the start), so it resolves hit-bearing
    /// files afterwards; this pins that resolution.
    #[test]
    fn search_hits_carry_surface_and_sidechain() {
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-x-p");
        let sub = proj
            .join("aaaaaaaa-1111-2222-3333-444444444444")
            .join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        // Timestamps must fall inside `wide_scope`: unlike a turns read, the
        // search path prunes by time.
        let now = tp_core::now_ms();
        let msg = |t: &str, ts: i64| {
            format!(
                r#"{{"type":"user","timestamp":"{}","message":{{"content":"{t}"}}}}"#,
                chrono_fmt(ts)
            )
        };
        let boundary = format!(
            r#"{{"type":"system","subtype":"compact_boundary","timestamp":"{}"}}"#,
            chrono_fmt(now.saturating_sub_ms(240_000).get())
        );
        std::fs::write(
            proj.join("aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
            format!(
                "{}
{}
{}
",
                msg(
                    "needle before the cut",
                    now.saturating_sub_ms(300_000).get()
                ),
                boundary,
                msg("needle after the cut", now.saturating_sub_ms(180_000).get()),
            ),
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-conf1.jsonl"),
            format!(
                r#"{{"type":"user","isSidechain":true,"timestamp":"{}","message":{{"content":"needle from the subagent"}}}}"#,
                chrono_fmt(now.saturating_sub_ms(120_000).get())
            ) + "
",
        )
        .unwrap();

        let scan = Retrieval::new(Box::new(ScanProvider::new(
            MACHINE,
            vec![Box::new(shipped("claude_code"))],
            vec![("claude_code".to_string(), root.path().to_path_buf())],
        )));

        let q = Query {
            text: "needle".into(),
            regex: false,
            include_thinking: false,
            limit: 10,
        };
        let flags = |r: &Retrieval| {
            let mut v: Vec<(String, bool, Surface)> = r
                .search(&q, &wide_scope())
                .unwrap()
                .items
                .into_iter()
                .map(|h| (h.excerpt().to_string(), h.sidechain, h.surface))
                .collect();
            // Providers rank differently; compare as sets keyed by excerpt.
            v.sort();
            v.into_iter()
                .map(|(e, side, surf)| {
                    // Snippet markers around the match are not part of the text.
                    (e.replace(['[', ']'], ""), side, surf)
                })
                .collect::<Vec<_>>()
        };
        let s_hits = flags(&scan);
        assert_eq!(s_hits.len(), 3, "{s_hits:?}");
        for (e, side, surf) in &s_hits {
            let (want_side, want_surf) = match e.as_str() {
                x if x.contains("before") => (false, Surface::Superseded),
                x if x.contains("after") => (false, Surface::Current),
                _ => (true, Surface::Current),
            };
            assert_eq!((*side, *surf), (want_side, want_surf), "{e}");
        }
    }

    #[test]
    fn an_anchored_marker_keeps_the_entry_it_names() {
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let msg = |id: &str, t: &str, ts: &str| {
            format!(
                r#"{{"type":"message","id":"{id}","timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        // The anchor points earlier than the marker, the shape pi data has,
        // and the reason a positional reading is wrong in the worst direction.
        std::fs::write(
            proj.join("2026-08-04T10-00-00-000Z_0199aaaa.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                msg("e1", "dropped", "2026-08-04T10:00:00+00:00"),
                msg("e2", "kept early", "2026-08-04T10:01:00+00:00"),
                msg("e3", "kept mid", "2026-08-04T10:02:00+00:00"),
                r#"{"type":"compaction","id":"c1","firstKeptEntryId":"e2","summary":"what e1 said","timestamp":"2026-08-04T10:03:00+00:00"}"#,
                msg("e4", "after", "2026-08-04T10:04:00+00:00"),
            ),
        )
        .unwrap();

        let scan = scan_turns("pi", "0199aaaa", root.path());
        use Surface::*;
        assert_eq!(
            surfaces(&scan),
            // e1 superseded; e2 (the anchor) kept; the compaction summary is
            // itself an indexed turn and lands current.
            [Superseded, Current, Current, Current, Current],
            "the anchor is kept, not cut: {:?}",
            scan.iter()
                .map(|t| (&t.text, t.surface))
                .collect::<Vec<_>>()
        );
    }
}

/// A runtime the scan holds no root for is a fact about the provider, not the
/// corpus, and must be reported as degraded. With no second provider to
/// redirect to, the note is the only thing separating "floonet cannot read
/// this" from "this session is empty".
#[test]
fn a_runtime_the_scan_cannot_see_is_degraded_not_empty() {
    let (_tmp, root) = fixture();
    let scan = scan_provider(&root);

    // An invented runtime, so no shipped descriptor can make it readable.
    let orphan = SessionId::new(MACHINE, "acme-harness", "session-0bdc4c9f");
    let got = scan
        .turns(&orphan, TurnCursor::Start, false, 10, Some(1 << 20))
        .unwrap();
    assert!(got.items.is_empty(), "there is genuinely nothing to return");
    let note = got.coverage.degraded.as_deref().unwrap_or("");
    assert!(
        note.contains("acme-harness"),
        "the note must name the runtime it cannot read, got: {note:?}"
    );
    // The note closes rather than redirects: it attributes the emptiness to
    // floonet, says no other route will do better, and names no flag that
    // does not exist.
    assert!(
        note.contains("NOT an empty session"),
        "the emptiness must be attributed to floonet, not the session, got: {note:?}"
    );
    assert!(
        note.contains("by any route") || note.contains("no other route"),
        "and must close the door instead of pointing at a route that no longer \
         exists, got: {note:?}"
    );
    assert!(
        !note.contains("--index"),
        "--index was removed; naming it sends the reader to a command that \
         fails, got: {note:?}"
    );

    // The honest empty stays empty: root present, file absent. A scan that
    // looked and found nothing is a complete answer; degrading it would make
    // the signal worthless by never being off.
    let missing = SessionId::new(MACHINE, "claude_code", "no-such-session");
    let got2 = scan
        .turns(&missing, TurnCursor::Start, false, 10, Some(1 << 20))
        .unwrap();
    assert!(got2.items.is_empty());
    assert!(
        got2.coverage.degraded.is_none(),
        "a session that is simply not there must not degrade, got: {:?}",
        got2.coverage.degraded
    );
}

// ── Inputs are part of the contract too ────────────────────────────────────
//
// The body above compares results. These cases pin how each provider treats
// its inputs: a parameter silently ignored by one backend is a divergence the
// result comparison cannot see.

/// `include_thinking` is honoured on `turns`, or it is not a parameter: a
/// caller who did not ask must not receive thinking from one backend and not
/// another.
#[test]
fn turns_honours_include_thinking_on_both_providers() {
    let (_tmp, root) = fixture();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);

    {
        let (name, r) = ("scan", scan_provider(&root));
        let with = r
            .turns(&sid, TurnCursor::Start, true, 50, Some(1 << 20))
            .unwrap();
        let without = r
            .turns(&sid, TurnCursor::Start, false, 50, Some(1 << 20))
            .unwrap();

        let has_thinking =
            |items: &[tp_core::turn::NormalizedTurn]| items.iter().any(|x| !x.thinking.is_empty());
        assert!(
            has_thinking(&with.items),
            "{name}: asking for thinking must return it — the fixture has one thinking block"
        );
        assert!(
            !has_thinking(&without.items),
            "{name}: thinking is opt-in at read time; this provider returned it unasked"
        );
    }
}

/// `Scope.runtimes` narrows every provider, or none. A backend that ignores
/// it answers the whole corpus with nothing in `Coverage` to say so: the
/// silent-widening twin of a false negative, an answer to a different question.
#[test]
fn scope_runtimes_narrows_both_providers_or_neither() {
    let (_tmp, root) = fixture();

    let scoped = |rt: &str| Scope {
        folder: None,
        since: std::time::Duration::from_secs(86_400 * 365),
        runtimes: vec![rt.to_string()],
        until: None,
    };

    {
        let (name, r) = ("scan", scan_provider(&root));
        let hits = r
            .search(&q("pagination", false), &scoped("claude_code"))
            .unwrap();
        assert!(
            !hits.items.is_empty(),
            "{name}: the fixture IS claude_code, so scoping to it must still match"
        );

        // A runtime not in the corpus must narrow to nothing; an ignored field
        // shows here.
        let none = r
            .search(&q("pagination", false), &scoped("no-such-runtime"))
            .unwrap();
        assert!(
            none.items.is_empty(),
            "{name}: scoping to a runtime with no sessions returned {} hit(s) — \
             the scope was ignored, and nothing in coverage said so",
            none.items.len()
        );
    }
}

/// `limit` is the caller's budget, not advisory: a backend that overshoots
/// it spends context the caller declined. Hits are collected per file, so
/// the cap has to hold when it falls inside a file with more matches after it.
#[test]
fn search_never_returns_more_than_limit_on_either_provider() {
    // A dedicated corpus: an overshoot needs a file whose matches continue
    // past the point the cap is reached, which the shared fixture cannot
    // express. Two files of six matching turns each does.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    for (n, dir) in [(1, "-Users-test-dev-a"), (2, "-Users-test-dev-b")] {
        let proj = root.join(dir);
        std::fs::create_dir_all(&proj).unwrap();
        let mut body = String::new();
        for i in 0..6 {
            let line = serde_json::json!({
                "type": "user",
                "cwd": format!("/Users/test/dev/{dir}"),
                "timestamp": chrono_fmt(1_780_000_000_000i64 + (n * 100 + i) as i64 * 1000),
                "message": {"content": format!("needle number {i} in file {n}")}
            });
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(
            proj.join(format!("aaaaaaaa-bbbb-cccc-dddd-00000000000{n}.jsonl")),
            body,
        )
        .unwrap();
    }

    {
        let (name, r) = ("scan", scan_provider(&root));
        // Several limits: a single value can land on a file boundary and miss
        // the overshoot entirely.
        for limit in 1..=4 {
            let mut q = q("needle", false);
            q.limit = limit;
            let got = r.search(&q, &wide_scope()).unwrap();
            assert!(
                got.items.len() <= limit,
                "{name}: limit {limit} returned {} hits — a caller's budget is not advisory",
                got.items.len()
            );
        }
    }
}

/// Windows that cut a session in the middle. The shared fixture is seconds
/// old, so every window there is "everything" or "nothing"; the edge worth
/// pinning is a long-lived file whose recent mtime survives the file-level
/// prune while holding almost nothing inside the window. A corpus spanning
/// several days with a quiet gap is the shape that reaches it.
#[test]
fn a_multi_day_session_cuts_the_same_way_on_both_providers() {
    const MID: &str = "77777777-6666-5555-4444-333333333333";

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-multi");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let d = 86_400_000i64;
    let h = 3_600_000i64;
    // Two fresh turns, two a week back, and an empty stretch between them.
    // The old turns sit hours inside their windows, not on the edges: a scope
    // re-derives `since` from its own `now`, so a turn exactly on the
    // boundary would test the clock, not the code.
    let turns = [
        ("needle fresh", now_ms - 60_000),
        ("needle threehours", now_ms - 3 * h),
        ("needle weekback", now_ms - 5 * d - 6 * h),
        ("needle weekplusone", now_ms - 6 * d - 6 * h),
    ];
    let body: String = turns
        .iter()
        .map(|(text, ts)| {
            serde_json::json!({
                "type": "user", "cwd": "/Users/test/dev/multi",
                "timestamp": chrono_fmt(*ts),
                "message": {"content": text}
            })
            .to_string()
                + "\n"
        })
        .collect();
    std::fs::write(proj.join(format!("{MID}.jsonl")), body).unwrap();

    let sid = SessionId::new(MACHINE, "claude_code", MID);

    {
        let (name, r) = ("scan", scan_provider(&root));
        // One window, as both the `turns` cursor and the `search` scope: the
        // two entry points must mean the same thing by it.
        let win = |since_ms: i64, before_ms: Option<i64>| {
            (
                TurnCursor::Window {
                    since_ms,
                    before_ms,
                },
                Scope {
                    folder: None,
                    since: Duration::from_millis((now_ms - since_ms) as u64),
                    runtimes: vec![],
                    until: before_ms,
                },
            )
        };
        let texts = |t: &Vec<tp_core::turn::NormalizedTurn>| {
            t.iter().map(|t| t.text.clone()).collect::<Vec<_>>()
        };

        // The quiet gap: a full day holding no turn, between the two old
        // ones. Empty is the honest answer, through both entry points.
        let (c, s) = win(now_ms - 6 * d, Some(now_ms - 5 * d - 12 * h));
        assert!(
            r.turns(&sid, c, false, 50, None).unwrap().items.is_empty(),
            "[{name}] the quiet gap holds no turn"
        );
        assert!(
            r.search(&q("needle", false), &s).unwrap().items.is_empty(),
            "[{name}] search must agree the quiet gap is empty"
        );

        // A window holding exactly one old turn: weekplusone is inside it,
        // weekback is newer than it and must stay out.
        let (c, s) = win(now_ms - 6 * d - 12 * h, Some(now_ms - 5 * d - 12 * h));
        let got = r.turns(&sid, c, false, 50, None).unwrap().items;
        assert_eq!(
            texts(&got),
            vec!["needle weekplusone".to_string()],
            "[{name}] the old window must hold exactly its one turn, and no newer one"
        );
        assert_eq!(
            r.search(&q("weekplusone", false), &s).unwrap().items.len(),
            1,
            "[{name}] search must find the turn the turns-read found"
        );
        assert!(
            r.search(&q("weekback", false), &s)
                .unwrap()
                .items
                .is_empty(),
            "[{name}] the newer turn outside the window must not leak in"
        );

        // A window holding both old turns.
        let (c, _s) = win(now_ms - 6 * d - 12 * h, Some(now_ms - 5 * d));
        let both = r.turns(&sid, c, false, 50, None).unwrap().items;
        assert_eq!(
            both.len(),
            2,
            "[{name}] a window spanning both old turns must return both"
        );

        // The fresh half, by duration alone.
        let (c, s) = win(now_ms - 3_600_000, None);
        assert_eq!(
            texts(&r.turns(&sid, c, false, 50, None).unwrap().items),
            vec!["needle fresh".to_string()],
            "[{name}] the last hour holds exactly the fresh turn — the file's \
             recent mtime must not drag its week-old turns into the window"
        );
        assert!(
            r.search(&q("weekback", false), &s)
                .unwrap()
                .items
                .is_empty(),
            "[{name}] search: same window, same answer"
        );

        let (c, s) = win(now_ms - 12 * 3_600_000, None);
        assert_eq!(
            r.turns(&sid, c, false, 50, None).unwrap().items.len(),
            2,
            "[{name}] the last twelve hours hold the two fresh turns"
        );
        assert_eq!(
            r.search(&q("threehours", false), &s).unwrap().items.len(),
            1,
            "[{name}] search: same window, same answer"
        );
    }
}

/// A live file mid-write: the final line is a torn JSON record with no
/// newline. The ingest contract says the torn tail is not parsed; this pins
/// that every provider agrees, including that the half-written line is not
/// searchable.
#[test]
fn a_torn_final_line_reads_the_same_on_both_providers() {
    const TORN: &str = "88888888-7777-6666-5555-444433333333";

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-torn");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let l0 = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/torn",
        "timestamp": chrono_fmt(now_ms - 2000),
        "message": {"content": "tornneedle one"}
    });
    let l1 = serde_json::json!({
        "type": "user",
        "timestamp": chrono_fmt(now_ms - 1000),
        "message": {"content": "tornneedle two"}
    });
    // Deliberately unparseable, and deliberately newline-free: a write cut off
    // mid-record.
    let torn = r#"{"type":"user","timestamp":"2026-01-01T00:00:00+00:00","message":{"content":"tornneedle THR"#;
    std::fs::write(
        proj.join(format!("{TORN}.jsonl")),
        format!("{l0}\n{l1}\n{torn}"),
    )
    .unwrap();

    let sid = SessionId::new(MACHINE, "claude_code", TORN);

    {
        let (name, r) = ("scan", scan_provider(&root));
        let all = r
            .turns(&sid, TurnCursor::Start, false, 10, None)
            .unwrap()
            .items;
        assert_eq!(
            all.len(),
            2,
            "[{name}] the torn tail must not read as a third turn"
        );
        assert_eq!(
            r.search(&q("tornneedle", false), &wide_scope())
                .unwrap()
                .items
                .len(),
            2,
            "[{name}] both complete turns must be searchable"
        );
        assert!(
            r.search(&q("THR", false), &wide_scope())
                .unwrap()
                .items
                .is_empty(),
            "[{name}] the half-written line must not be searchable"
        );
    }
}

/// `cwd` stated once, on a head line the needle does not match: every hit
/// must still carry it. Runtimes that state cwd only in a head record have
/// this shape, and a prefilter that skips the head would leave every hit of
/// the file reading as "directory unknown". Scan-only: it pins the scan's own
/// contract, not a cross-provider equality.
#[test]
fn a_head_only_cwd_reaches_hits_the_head_does_not_match() {
    const HEAD: &str = "66666666-5555-4444-3333-222222222222";

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-cwdhead");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let head = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/cwdhead",
        "timestamp": chrono_fmt(now_ms - 2000),
        "message": {"content": "setup complete"}
    });
    let body = serde_json::json!({
        "type": "user",
        "timestamp": chrono_fmt(now_ms - 1000),
        "message": {"content": "headneedle only here"}
    });
    let body2 = serde_json::json!({
        "type": "user",
        "timestamp": chrono_fmt(now_ms - 500),
        "message": {"content": "headneedle again here"}
    });
    std::fs::write(
        proj.join(format!("{HEAD}.jsonl")),
        format!("{head}\n{body}\n{body2}\n"),
    )
    .unwrap();

    let got = scan_provider(&root)
        .search(&q("headneedle", false), &wide_scope())
        .unwrap();
    assert_eq!(got.items.len(), 2, "both body lines must match");
    for h in &got.items {
        assert_eq!(
            h.cwd.as_deref(),
            Some("/Users/test/dev/cwdhead"),
            "the head states the cwd once; every hit of the file must carry it"
        );
    }
}
