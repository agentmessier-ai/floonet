//! Every generated pi corpus, through the real ingest path, checked against
//! the expectation the generator wrote beside it.
//!
//! The fixtures under `tests/fixtures/pi/` are produced from the pi session
//! spec; this file only executes and compares, so a scenario is added by
//! adding a directory. The expectation is derived from the input rather than
//! recorded from a run: a golden file asserts that the code still does what it
//! does, and a wrong answer would stay wrong for as long as it was the answer.
//!
//! Read through the scan provider with the root passed in, which is how a pi
//! transcript is read in production and keeps scenarios independent of the
//! environment.

use serde_json::Value;
use std::path::{Path, PathBuf};
use tp_core::retrieval::{Scope, TurnCursor};
use tp_core::SessionId;
use tp_search::{Retrieval, ScanProvider};

const MACHINE: &str = "m-pi-spec";

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi")
}

/// One scenario, laid out exactly as the generator writes it.
struct Scenario {
    name: String,
    /// `scenario.json`. `cwd` names the directory the file is laid down in
    /// and is expected to also appear in the header line, which is where the
    /// adapter reads it from.
    meta: Value,
    /// `expect.json`
    expect: Value,
    raw: Vec<u8>,
    file_name: String,
}

fn load_all() -> Vec<Scenario> {
    let dir = fixtures_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(p.join("scenario.json"))
                .unwrap_or_else(|e| panic!("{name}: scenario.json: {e}")),
        )
        .unwrap_or_else(|e| panic!("{name}: scenario.json is not JSON: {e}"));
        let expect: Value = serde_json::from_str(
            &std::fs::read_to_string(p.join("expect.json"))
                .unwrap_or_else(|e| panic!("{name}: expect.json: {e}")),
        )
        .unwrap_or_else(|e| panic!("{name}: expect.json is not JSON: {e}"));
        // Read as bytes: a scenario may hold invalid UTF-8 or a torn final
        // line, and the adapter must be the thing that meets it.
        let raw = std::fs::read(p.join("session.jsonl"))
            .unwrap_or_else(|e| panic!("{name}: session.jsonl: {e}"));
        let file_name = meta["file_name"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: scenario.json needs `file_name`"))
            .to_string();
        out.push(Scenario {
            name,
            meta,
            expect,
            raw,
            file_name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// pi encodes the working directory into the directory name with separators
/// replaced. The adapter reads `cwd` from the header line; the name is what
/// `--folder` matching sees. Mirrored here rather than imported so a change
/// to the encoding fails the folder assertions instead of silently following.
fn encode_cwd(cwd: &str) -> String {
    let mut s = String::from("-");
    for c in cwd.chars() {
        s.push(if c.is_alphanumeric() { c } else { '-' });
    }
    s
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

fn list_at<'a>(v: &'a Value, k: &str) -> Vec<&'a str> {
    v.get(k)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn every_generated_pi_scenario_lands_as_specified() {
    let scenarios = load_all();
    assert!(
        !scenarios.is_empty(),
        "no scenarios in {} — the pi-fixture agent has not run, or wrote elsewhere",
        fixtures_dir().display()
    );

    let mut failures: Vec<String> = Vec::new();

    for s in &scenarios {
        let tmp = tempfile::tempdir().unwrap();
        let corpus = tmp.path().join("corpus");
        let cwd = str_at(&s.meta, "cwd").unwrap_or("/Users/test/dev/demo");
        let proj = corpus.join(encode_cwd(cwd));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(&s.file_name), &s.raw).unwrap();

        // The shipped descriptor, loaded from disk: the file is what is under
        // test, and a hand-built config would let it rot unnoticed.
        let shipped = tp_ingest::adapter::decl::load_configs(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d"),
        )
        .into_iter()
        .find(|c| c.id == "pi")
        .expect("shipped pi.toml must load");
        let r = Retrieval::new(Box::new(ScanProvider::new(
            MACHINE,
            vec![Box::new(tp_ingest::adapter::decl::DeclAdapter::new(
                shipped,
            ))],
            vec![("pi".to_string(), corpus.clone())],
        )));

        let mut fail = |m: String| failures.push(format!("[{}] {m}", s.name));

        // Unbounded: a scenario's timestamps are fixed absolute instants, so a
        // window here would silently decide which fixtures are visible.
        let scope = Scope {
            folder: None,
            since: std::time::Duration::from_secs(3650 * 24 * 3600),
            runtimes: vec![],
            until: None,
        };
        let sessions = r.sessions(&scope, 50).unwrap().items;

        let want_sessions = s.expect["session_count"].as_i64().unwrap_or(1);
        if sessions.len() as i64 != want_sessions {
            fail(format!(
                "expected {want_sessions} session(s), read {}",
                sessions.len()
            ));
            continue;
        }
        if want_sessions == 0 {
            continue;
        }

        let sess = &sessions[0];
        let want = &s.expect["session"];

        if let Some(w) = str_at(want, "cwd") {
            if sess.cwd.as_deref() != Some(w) {
                fail(format!("cwd: want {w:?}, got {:?}", sess.cwd));
            }
        }
        // `title` distinguishes "not asserted" (key absent) from "must be this
        // value" (present, possibly null).
        if want.get("title").is_some() {
            let expected = want["title"].as_str();
            if sess.title.as_deref() != expected {
                fail(format!("title: want {expected:?}, got {:?}", sess.title));
            }
        }

        let sid = SessionId::parse(&sess.id).expect("provider returns a composite id");
        let turns = r
            .turns(&sid, TurnCursor::Start, true, 10_000, None)
            .unwrap()
            .items;

        let want_turns = s.expect["turns"].as_array().cloned().unwrap_or_default();
        if turns.len() != want_turns.len() {
            fail(format!(
                "expected {} turn(s), read {}: {:?}",
                want_turns.len(),
                turns.len(),
                turns.iter().map(|t| (&t.role, &t.text)).collect::<Vec<_>>()
            ));
            continue;
        }

        for (i, (got, w)) in turns.iter().zip(want_turns.iter()).enumerate() {
            let at = |m: String| format!("turn {i}: {m}");
            if let Some(want_role) = str_at(w, "role") {
                let got_role = match got.role {
                    tp_core::turn::Role::User => "user",
                    tp_core::turn::Role::Assistant => "assistant",
                };
                if got_role != want_role {
                    fail(at(format!("role: want {want_role:?}, got {got_role:?}")));
                }
            }
            if let Some(t) = str_at(w, "text_is") {
                if got.text != t {
                    fail(at(format!("text: want {t:?}, got {:?}", got.text)));
                }
            }
            for needle in list_at(w, "text_contains") {
                if !got.text.contains(needle) {
                    fail(at(format!("text must contain {needle:?}: {:?}", got.text)));
                }
            }
            for needle in list_at(w, "text_absent") {
                if got.text.contains(needle) {
                    fail(at(format!("text must NOT contain {needle:?}")));
                }
            }
            if let Some(t) = str_at(w, "thinking_contains") {
                if !got.thinking.contains(t) {
                    fail(at(format!(
                        "thinking must contain {t:?}: {:?}",
                        got.thinking
                    )));
                }
            }
            if let Some(b) = w.get("thinking_opaque").and_then(|x| x.as_bool()) {
                if got.thinking_opaque != b {
                    fail(at(format!(
                        "thinking_opaque: want {b}, got {}",
                        got.thinking_opaque
                    )));
                }
            }
            let want_tools = list_at(w, "tools");
            if w.get("tools").is_some() {
                let got_tools: Vec<&str> = got.tool_calls.iter().map(|t| t.name.as_str()).collect();
                if got_tools != want_tools {
                    fail(at(format!("tools: want {want_tools:?}, got {got_tools:?}")));
                }
            }
            if let Some(sf) = str_at(w, "surface") {
                let got_sf = format!("{:?}", got.surface).to_lowercase();
                if got_sf != sf {
                    fail(at(format!("surface: want {sf:?}, got {got_sf:?}")));
                }
            }
        }

        // Whole-corpus negatives, asserted across the concatenation so a
        // scenario cannot pass by leaking into a field its per-turn
        // expectation did not name.
        let haystack: String = turns
            .iter()
            .map(|t| {
                format!(
                    "{}\u{1}{}\u{1}{}",
                    t.text,
                    t.thinking,
                    t.tool_calls
                        .iter()
                        .map(|c| format!(
                            "{}{}",
                            c.name,
                            c.input_digest.clone().unwrap_or_default()
                        ))
                        .collect::<String>()
                )
            })
            .collect();
        for needle in list_at(&s.expect, "absent_everywhere") {
            if haystack.contains(needle) {
                fail(format!("{needle:?} reached storage; it must never"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} scenario assertion(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
