//! A compressed transcript must read back as all of its lines.
//!
//! The fixtures are concatenated zstd frames, the shape of a file grown one
//! appended frame per write batch. The failure guarded against is a decode
//! that succeeds on frame one and reports the result as the whole file.

use std::path::PathBuf;
use tp_ingest::source;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/zstd")
        .join(name)
}

#[test]
fn every_frame_is_read_not_just_the_first() {
    let got = source::read_text(&fixture("three-frames.jsonl.zstd")).unwrap();
    let want = std::fs::read_to_string(fixture("three-frames.jsonl")).unwrap();
    // Byte-for-byte, so a dropped, reordered or duplicated frame fails here
    // rather than passing a line count.
    assert_eq!(got, want);
    assert_eq!(got.lines().count(), 4, "frames 2 and 3 were dropped");
}

#[test]
fn a_frame_still_being_written_is_left_unread() {
    // The tail is a partial frame: keep what is whole and leave the rest for
    // the next pass, as `read_chunk` does with a partial line.
    let got = source::read_text(&fixture("torn-tail.jsonl.zstd")).unwrap();
    assert_eq!(got.lines().count(), 3, "the two whole frames must survive");
    assert!(got.starts_with(r#"{"type":"session""#));
}

#[test]
fn a_plaintext_transcript_is_passed_through() {
    let p = fixture("three-frames.jsonl");
    assert!(!source::is_compressed(&p));
    assert_eq!(
        source::read_text(&p).unwrap(),
        std::fs::read_to_string(&p).unwrap()
    );
}

/// Read-only, opt-in, skipped when absent: invented frames cannot prove the
/// decoder handles what dsh actually emits.
#[test]
fn a_real_dsh_session_reads_back_whole() {
    let Ok(root) = std::env::var("TP_DSH_SESSIONS") else {
        eprintln!("skipped: set TP_DSH_SESSIONS to a dsh sessions root");
        return;
    };
    let mut checked = 0;
    for f in walk(std::path::Path::new(&root)) {
        let text = source::read_text(&f).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert!(lines.len() > 1, "{f:?} decoded to {} line(s)", lines.len());
        let head: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(head["type"], "session", "{f:?} has no header line");
        for (i, l) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(l)
                .unwrap_or_else(|e| panic!("{f:?} line {i}: {e}"));
        }
        eprintln!("  {} lines  {}", lines.len(), f.display());
        checked += 1;
    }
    assert!(checked > 0, "no session.jsonl.zstd under {root}");
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.file_name().is_some_and(|n| n == "session.jsonl.zstd") {
            out.push(p);
        }
    }
    out
}
