//! The shipped dsh descriptor, against dsh's on-disk shape.
//!
//! The fixture is invented. What it reproduces is what makes dsh differ from
//! the other runtimes: two message shapes at two content paths, a numeric
//! timestamp, and a file whose name carries no id.

use std::path::{Path, PathBuf};
use tp_ingest::adapter::decl::{DeclAdapter, DeclConfig};
use tp_ingest::Adapter;

fn adapter() -> DeclAdapter {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/dsh.toml"),
    )
    .unwrap();
    DeclAdapter::new(toml::from_str::<DeclConfig>(&text).expect("dsh.toml must parse"))
}

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsh")
}

#[test]
fn the_shipped_descriptor_parses() {
    let a = adapter();
    assert_eq!(a.id(), "dsh");
}

#[test]
fn a_session_is_found_two_levels_down_and_named_by_its_directory() {
    let found = adapter().discover(&corpus()).unwrap();
    let mut ids: Vec<_> = found.iter().map(|s| s.native_id.clone()).collect();
    ids.sort();
    // Both files are called `session.jsonl.zstd`; a rule over the stem would
    // collapse them onto one id.
    assert_eq!(ids, vec!["session-aaa11111", "session-bbb22222"]);
}

#[test]
fn both_message_shapes_are_read_from_their_own_paths() {
    let a = adapter();
    let src = adapter()
        .discover(&corpus())
        .unwrap()
        .into_iter()
        .find(|s| s.native_id == "session-aaa11111")
        .unwrap();
    let chunk = a.parse_from(&src.path, 0).unwrap();
    let turns = &chunk.turns;

    let texts: Vec<_> = turns.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["count the ships", "There are four."],
        "a user turn read through the assistant's nested path, or vice versa"
    );

    // The assistant turn carries reasoning and a tool call; a `text_path` rule
    // would return its text and drop both.
    let asst = &turns[1];
    assert_eq!(asst.thinking, "Four, by my count.");
    assert_eq!(
        asst.tool_calls
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bash"]
    );
    assert_eq!(asst.tokens_in, Some(120));
    assert_eq!(asst.tokens_out, Some(8));

    // Epoch millis, read as-is; a turn with no timestamp is one no time window
    // can find.
    assert_eq!(turns[0].ts, Some(1_700_000_001_000));
    assert_eq!(asst.ts, Some(1_700_000_002_000));
}

#[test]
fn the_session_states_its_own_cwd_and_title() {
    let a = adapter();
    let src = a
        .discover(&corpus())
        .unwrap()
        .into_iter()
        .find(|s| s.native_id == "session-aaa11111")
        .unwrap();
    let chunk = a.parse_from(&src.path, 0).unwrap();
    assert_eq!(chunk.meta.cwd.as_deref(), Some("/w/harbour"));
    assert_eq!(chunk.meta.title_ai.as_deref(), Some("counting ships"));
}

#[test]
fn a_session_can_be_addressed_by_its_id() {
    let a = adapter();
    let hit = a.locate(&corpus(), "session-bbb22222").unwrap().unwrap();
    assert_eq!(hit.native_id, "session-bbb22222");
    assert!(a.locate(&corpus(), "session-nope").unwrap().is_none());
}

#[test]
fn the_real_corpus_reads_if_it_is_there() {
    let Ok(root) = std::env::var("TP_DSH_SESSIONS") else {
        eprintln!("skipped: set TP_DSH_SESSIONS to a dsh sessions root");
        return;
    };
    let a = adapter();
    let found = a.discover(Path::new(&root)).unwrap();
    assert!(!found.is_empty(), "no dsh sessions under {root}");
    for src in &found {
        let chunk = a.parse_from(&src.path, 0).unwrap();
        let with_text = chunk.turns.iter().filter(|t| !t.text.is_empty()).count();
        let with_think = chunk
            .turns
            .iter()
            .filter(|t| !t.thinking.is_empty())
            .count();
        let with_tools = chunk
            .turns
            .iter()
            .filter(|t| !t.tool_calls.is_empty())
            .count();
        let untimed = chunk.turns.iter().filter(|t| t.ts.is_none()).count();
        eprintln!(
            "  {} turns (text {with_text}, thinking {with_think}, tools {with_tools}) cwd={:?} title={:?}  {}",
            chunk.turns.len(), chunk.meta.cwd, chunk.meta.title_ai, src.native_id,
        );
        assert!(!chunk.turns.is_empty(), "{:?} parsed to nothing", src.path);
        assert_eq!(
            untimed, 0,
            "{:?} has {untimed} turns with no timestamp",
            src.path
        );
        assert!(chunk.meta.cwd.is_some(), "{:?} states no cwd", src.path);
    }
}

#[test]
fn a_format_version_this_build_does_not_know_is_refused() {
    let a = adapter();
    // v0 is the version the descriptor declares.
    assert_eq!(
        a.unsupported_reason(r#"{"type":"session","version":0,"id":"s","cwd":"/w"}"#),
        None
    );
    // The refusal names the version seen and the versions read, so the reader
    // can tell which side moved.
    let why = a
        .unsupported_reason(r#"{"type":"session","version":1,"id":"s","cwd":"/w"}"#)
        .expect("v1 must be refused");
    assert!(why.contains("version 1"), "{why}");
    assert!(why.contains('0'), "must say what it does read: {why}");
    // Only the header states a version; an event line without one is not a
    // refusal.
    assert_eq!(
        a.unsupported_reason(r#"{"type":"user/message","seq":1,"time":1,"data":{}}"#),
        None
    );
}

#[test]
fn a_runtime_that_states_no_version_is_never_refused() {
    // A descriptor that declares no version has nothing to check; the gate
    // must be inert for it.
    for id in ["claude_code", "pi", "codex"] {
        let a = tp_ingest::builtin(id);
        assert_eq!(
            a.unsupported_reason(r#"{"type":"session","version":9999}"#),
            None,
            "{id} has no version gate and must not refuse anything"
        );
    }
}
