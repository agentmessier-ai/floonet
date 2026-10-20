//! Incremental JSONL reading, once, for every adapter.
//!
//! Every runtime writes one JSON object per line and appends while a session
//! is live, so every adapter needs the same loop: read from a byte offset,
//! split on newlines, report how far it got.
//!
//! The rule that makes this worth centralising is the torn final line. A live
//! transcript can be read mid-write, so the last line may be half an object.
//! It is neither parsed nor counted in `new_offset`: counting it would resume
//! the next poll from the middle of a line, and that turn would be lost
//! permanently.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tp_core::turn::{CompactionBoundary, NormalizedTurn, Role, SessionMeta};

use super::ParseChunk;

/// How much of the first user message becomes the fallback title. It bounds a
/// display string, so the value matters less than there being one of it: the
/// scan backend truncates the same way, and the two disagreeing would make a
/// session's title depend on which provider answered.
pub const TITLE_CHARS: usize = 200;

/// Whether a record is a compaction marker, given the number of turns seen
/// before it, which a positional boundary needs and an anchored one ignores.
type BoundaryFn<'a> = &'a dyn Fn(&Value, usize) -> Option<CompactionBoundary>;

/// Read complete JSONL records from `offset` onward.
///
/// The closures are the only runtime-specific parts: where the working
/// directory and title are recorded, how one object becomes a turn, and how a
/// compaction marker is recognised. Byte accounting, the torn line, blank and
/// unparseable lines, and which session metadata may be set live here.
///
/// Session metadata is collected only on the `offset == 0` pass: a later poll
/// re-reading the tail must not retitle the session from a later message.
pub fn read_chunk(
    path: &Path,
    offset: u64,
    cwd_at: impl Fn(&Value) -> Option<String>,
    parse_entry: impl Fn(&Value) -> Option<NormalizedTurn>,
    title_at: impl Fn(&Value) -> Option<(tp_core::turn::TitleSource, String)>,
    // `None` for the whole closure means this runtime's marker is unknown,
    // which is not "there was none" — see `ParseChunk::tracks_compaction`.
    compaction_at: Option<BoundaryFn<'_>>,
) -> Result<ParseChunk> {
    // A compressed transcript has no usable byte offset into the file; the
    // offset indexes decompressed content, so it is decoded whole and sliced.
    let bytes = if crate::source::is_compressed(path) {
        let all = crate::source::read_bytes(path).with_context(|| format!("open {path:?}"))?;
        all.get(offset as usize..).unwrap_or_default().to_vec()
    } else {
        let mut file = File::open(path).with_context(|| format!("open {path:?}"))?;
        file.seek(SeekFrom::Start(offset))?;
        // Bytes, not `read_to_string`: a write can be torn in the middle of a
        // multi-byte character, and validating the whole buffer would fail the
        // chunk, complete lines included.
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        bytes
    };

    let mut turns = Vec::new();
    let mut compaction = Vec::new();
    let mut meta = SessionMeta::default();
    let mut consumed: u64 = 0;
    let first_pass = offset == 0;

    // Byte-exact splitting, newline included, so `consumed` maps onto file
    // offsets. Running out of newlines leaves the torn tail unread and
    // uncounted, so the next poll re-reads it whole.
    let mut rest = bytes.as_slice();
    while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
        let line = &rest[..nl];
        consumed += (nl + 1) as u64;
        rest = &rest[nl + 1..];

        // A complete line that is not valid UTF-8 is consumed and skipped, like
        // one that is not valid JSON: the newline proves the writer finished
        // it, and stopping would wedge the session on it forever.
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };

        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if first_pass && meta.cwd.is_none() {
            meta.cwd = cwd_at(&v);
        }

        // A title entry is not gated on `first_pass`: a rename can happen at
        // any point, so the newest statement wins. The opposite rule from
        // `title_derived` below, which is a guess from the opening message.
        if let Some((source, title)) = title_at(&v) {
            meta.set_title(source, title);
        }

        // Checked before parsing the record as a turn, and without skipping
        // it: a marker that carries a summary may also be a turn. The boundary
        // is recorded against the turns seen so far, so such a marker lands
        // after its own boundary and stays current, as new context should.
        if let Some(boundary_of) = compaction_at {
            if let Some(b) = boundary_of(&v, turns.len()) {
                compaction.push(b);
            }
        }

        if let Some(turn) = parse_entry(&v) {
            // The fallback title, which loses to any native title in the
            // read-time coalesce.
            if first_pass
                && meta.title_derived.is_none()
                && turn.role == Role::User
                && !turn.text.is_empty()
            {
                meta.title_derived = Some(turn.text.chars().take(TITLE_CHARS).collect());
            }
            if meta.started_at.is_none() {
                meta.started_at = turn.ts;
            }
            turns.push(turn);
        }
    }

    Ok(ParseChunk {
        turns,
        new_offset: offset + consumed,
        meta,
        compaction,
        tracks_compaction: compaction_at.is_some(),
    })
}

/// Normalise one line, for the scan backend, by the same per-line rules
/// `read_chunk` applies: blank is nothing, unparseable is nothing, otherwise
/// the runtime's own parser decides. Both backends must agree on what a line
/// means, or a session reads differently depending on which provider answered.
pub fn parse_one(
    line: &str,
    parse_entry: impl Fn(&Value) -> Option<NormalizedTurn>,
) -> Option<NormalizedTurn> {
    if line.trim().is_empty() {
        return None;
    }
    parse_entry(&serde_json::from_str::<Value>(line).ok()?)
}

/// A top-level `cwd`, the default location.
pub fn cwd_field(v: &Value) -> Option<String> {
    v.get("cwd").and_then(|c| c.as_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn user(v: &Value) -> Option<NormalizedTurn> {
        let text = v.get("text")?.as_str()?.to_string();
        Some(NormalizedTurn {
            role: Role::User,
            ts: v.get("ts").and_then(|t| t.as_i64()),
            text,
            thinking: String::new(),
            thinking_opaque: false,
            tool_calls: Vec::new(),
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Default::default(),
        })
    }

    fn write(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, p)
    }

    fn read(p: &Path, offset: u64) -> ParseChunk {
        read_chunk(p, offset, cwd_field, user, |_| None, None).unwrap()
    }

    /// A poll that lands inside a write sees half an object; consuming it
    /// would resume the next poll mid-line and lose that turn permanently.
    #[test]
    fn a_torn_final_line_is_neither_parsed_nor_consumed() {
        let whole = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(&format!("{whole}{{\"ts\":2,\"text\":\"tor"));

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1, "the torn line must not be parsed");
        assert_eq!(
            chunk.new_offset,
            whole.len() as u64,
            "new_offset must stop at the last complete line"
        );
    }

    /// The other half of the rule: once the writer finishes the line, resuming
    /// from `new_offset` picks it up whole. Without this, holding the line
    /// back could be satisfied by dropping the turn entirely.
    #[test]
    fn the_torn_line_is_recovered_when_the_write_completes() {
        let head = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(&format!("{head}{{\"ts\":2,\"text\":\"tor"));
        let first = read(&p, 0);

        // The writer finishes the record.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"n\"}\n").unwrap();

        let second = read(&p, first.new_offset);
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].text, "torn");
    }

    /// A complete-but-unparseable line is consumed. The newline proves the
    /// writer finished it, so stopping would wedge the session on one bad
    /// record forever.
    #[test]
    fn a_complete_line_that_is_not_json_is_skipped_not_retried() {
        let body = "not json at all\n{\"ts\":1,\"text\":\"after\"}\n";
        let (_d, p) = write(body);

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.turns[0].text, "after");
        assert_eq!(chunk.new_offset, body.len() as u64, "both lines consumed");
    }

    #[test]
    fn blank_lines_are_consumed_and_ignored() {
        let body = "\n\n{\"ts\":1,\"text\":\"x\"}\n\n";
        let (_d, p) = write(body);

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.new_offset, body.len() as u64);
    }

    /// Metadata belongs to the session, not to the chunk; a later poll must
    /// not retitle it.
    #[test]
    fn session_metadata_is_taken_only_on_the_first_pass() {
        let head = "{\"cwd\":\"/w\",\"ts\":1,\"text\":\"the title\"}\n";
        let (_d, p) = write(&format!(
            "{head}{{\"cwd\":\"/elsewhere\",\"ts\":2,\"text\":\"later\"}}\n"
        ));

        let first = read(&p, 0);
        assert_eq!(first.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(first.meta.title_derived.as_deref(), Some("the title"));
        assert_eq!(first.meta.started_at, Some(1));

        let second = read(&p, head.len() as u64);
        assert_eq!(second.meta.cwd, None, "a resumed read must not retitle");
        assert_eq!(second.meta.title_derived, None);
    }

    /// Offsets are byte counts, not character counts — a multi-byte line would
    /// otherwise leave the next read starting mid-character.
    #[test]
    fn offsets_are_bytes_even_when_the_text_is_not_ascii() {
        let body = "{\"ts\":1,\"text\":\"a €10 line\"}\n{\"ts\":2,\"text\":\"second\"}\n";
        let (_d, p) = write(body);

        let chunk = read(&p, 0);
        assert_eq!(chunk.new_offset, body.len() as u64);
        assert_eq!(chunk.turns.len(), 2);
    }

    /// A write torn in the middle of a multi-byte character must not fail the
    /// read of the complete lines before it.
    #[test]
    fn a_torn_multibyte_character_does_not_lose_the_complete_lines_before_it() {
        let whole = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(whole);
        // The first two bytes of a three-byte character the writer has not
        // finished emitting.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[0xE2, 0x82]).unwrap();

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1, "the complete line must survive");
        assert_eq!(
            chunk.new_offset,
            whole.len() as u64,
            "the torn character must not be consumed"
        );
    }

    /// The recovery half: once the writer finishes the character, the line
    /// containing it is read whole.
    #[test]
    fn the_torn_character_is_recovered_when_the_write_completes() {
        let head = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(head);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"ts\":2,\"text\":\"").unwrap();
        f.write_all(&[0xE2, 0x82]).unwrap();
        let first = read(&p, 0);
        assert_eq!(first.turns.len(), 1);

        f.write_all(&[0xAC]).unwrap();
        f.write_all(b"\"}\n").unwrap();
        let second = read(&p, first.new_offset);
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].text, "€");
    }

    /// A complete line that is not valid UTF-8 is corruption, not a write in
    /// progress: consumed and skipped, like a complete line that is not JSON.
    #[test]
    fn a_complete_line_that_is_not_utf8_is_skipped_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = File::create(&p).unwrap();
        f.write_all(&[0xFF, 0xFE]).unwrap();
        f.write_all(b"\n{\"ts\":1,\"text\":\"after\"}\n").unwrap();
        drop(f);

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.turns[0].text, "after");
        assert_eq!(
            chunk.new_offset,
            std::fs::metadata(&p).unwrap().len(),
            "both lines consumed"
        );
    }

    /// An empty file is a session that has not been written to yet, not an
    /// error and not a torn line.
    #[test]
    fn an_empty_file_yields_nothing_and_advances_nothing() {
        let (_d, p) = write("");
        let chunk = read(&p, 0);
        assert!(chunk.turns.is_empty());
        assert_eq!(chunk.new_offset, 0);
    }
}
