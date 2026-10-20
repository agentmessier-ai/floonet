//! Reading a transcript off disk, whatever physical encoding it is in.
//!
//! A compressed transcript is a concatenation of zstd frames, one appended per
//! write batch, so it is decoded frame by frame until the remainder no longer
//! starts with a frame magic. A single-frame decoder would return `Ok` with
//! the first frame's bytes and silently stop.
//!
//! A remainder that is not a frame magic is left unread: a frame still being
//! written is not yet a frame, and the next pass re-reads it whole. This is
//! the same torn-tail rule `jsonl::read_chunk` applies to a partial line.

use std::io::Read;
use std::path::Path;

/// The four bytes every zstd frame starts with (0xFD2FB528, little-endian).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Whether this path names a compressed transcript, by extension.
pub fn is_compressed(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("zst") | Some("zstd")
    )
}

/// Every frame in a concatenated zstd stream, decoded in order.
///
/// A frame that fails to decode ends the walk and keeps what came before it:
/// a half-written frame has a valid magic and a truncated body, and failing
/// the whole read would discard every complete frame ahead of it. The cost is
/// that a torn tail and mid-stream corruption are indistinguishable; both
/// yield the prefix. Only a failure on the first frame propagates, because an
/// empty result would misdescribe a file that is not a zstd stream at all.
fn decode_frames(mut rest: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while rest.len() >= 4 && rest[..4] == ZSTD_MAGIC {
        let before = rest.len();
        let decoded = ruzstd::StreamingDecoder::new(rest)
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|mut dec| {
                let mut buf = Vec::new();
                dec.read_to_end(&mut buf)?;
                Ok((buf, dec.into_inner()))
            });
        match decoded {
            Ok((buf, remainder)) => {
                out.extend_from_slice(&buf);
                rest = remainder;
            }
            Err(e) if out.is_empty() => return Err(e),
            Err(_) => break,
        }
        // A frame that consumed nothing ends the walk; retrying would spin
        // forever on a crafted input.
        if rest.len() >= before {
            break;
        }
    }
    Ok(out)
}

/// A transcript's bytes, decompressed if it is stored compressed.
///
/// Bytes, not `String`: a live transcript can be torn mid-write in the middle
/// of a multi-byte character, so the caller splits on newlines itself rather
/// than validating the whole buffer up front.
pub fn read_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    if is_compressed(path) {
        decode_frames(&raw)
    } else {
        Ok(raw)
    }
}

/// A transcript's text, lossily decoded. For readers that only want lines and
/// carry no torn-tail obligation.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    Ok(String::from_utf8_lossy(&read_bytes(path)?).into_owned())
}
