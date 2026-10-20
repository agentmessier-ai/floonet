//! The one parser engine every descriptor drives. The vocabulary it
//! is configured by lives in `config.rs`.

use super::config::*;
use crate::adapter::{digest_input, Adapter, SourceFile};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use tp_core::turn::{CompactionBoundary, NormalizedTurn, ParseChunk, Role, ToolCallDigest};

fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Every directory exactly `depth` levels below `root`, plus every `nested_dir`
/// below those. Exact, not "up to": a date-partitioned tree keeps nothing at
/// the intermediate levels, and accepting files there would widen what a
/// shallower runtime discovers.
fn dirs_at_depth(root: &Path, depth: usize, nested_dir: &str) -> Vec<std::path::PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    next.push(p);
                }
            }
        }
        level = next;
    }
    if nested_dir.is_empty() {
        return level;
    }
    // Walk wide, collect narrow: the nesting path passes through directories
    // that hold no transcripts themselves, so only the named ones are kept.
    let mut out = level.clone();
    let mut frontier = level;
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for dir in &frontier {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().and_then(|n| n.to_str()) == Some(nested_dir) {
                        out.push(p.clone());
                    }
                    next.push(p);
                }
            }
        }
        frontier = next;
    }
    out
}

pub struct DeclAdapter {
    cfg: DeclConfig,
    id: &'static str,
}

impl DeclAdapter {
    /// `id` is leaked to `&'static str` because `Adapter::id` returns one.
    /// Adapters are constructed once per process and live for its duration,
    /// so the leak is bounded.
    pub fn new(cfg: DeclConfig) -> Self {
        let id: &'static str = Box::leak(cfg.id.clone().into_boxed_str());
        Self { cfg, id }
    }

    /// The session id this path names. `ParentDir` needs the whole path, so
    /// the stem rules are reached through here rather than called directly.
    fn native_id_from_path(&self, path: &Path) -> Option<String> {
        if self.cfg.native_id == NativeIdRule::ParentDir {
            let name = path.parent()?.file_name()?.to_str()?;
            return (!name.is_empty()).then(|| name.to_string());
        }
        let name = path.file_name().and_then(|s| s.to_str())?;
        // The file name with the suffix cut, not `file_stem`, which would
        // leave `a.jsonl` for `a.jsonl.zstd`. Longest match first, so `.jsonl`
        // never shadows `.jsonl.zstd`.
        let stem = self
            .cfg
            .file_suffix
            .iter()
            .filter_map(|sfx| name.strip_suffix(sfx.as_str()))
            .max_by_key(|s| name.len() - s.len())
            .unwrap_or(name);
        self.native_id_from_stem(stem)
    }

    /// Whether this path is a transcript of this runtime, by filename suffix.
    fn claims_file(&self, path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            self.cfg
                .file_suffix
                .iter()
                .any(|sfx| n.ends_with(sfx.as_str()))
        })
    }

    fn native_id_from_stem(&self, stem: &str) -> Option<String> {
        let id = match self.cfg.native_id {
            // `ParentDir` is handled in `native_id_from_path`.
            NativeIdRule::ParentDir | NativeIdRule::Stem => stem,
            // Degrade to the whole stem rather than dropping the file: a
            // naming change should make a session searchable under an odd id,
            // never silently invisible.
            NativeIdRule::AfterUnderscore => stem.split_once('_').map(|(_, id)| id).unwrap_or(stem),
            NativeIdRule::TrailingUuid => trailing_uuid(stem).unwrap_or(stem),
        };
        (!id.is_empty()).then(|| id.to_string())
    }

    fn ts_of(&self, v: &Value) -> Option<i64> {
        let raw = at(v, &self.cfg.ts_path)?;
        // A number is epoch milliseconds; a string is RFC3339. A turn with no
        // timestamp is one no time window can find.
        if let Some(ms) = raw.as_i64() {
            return Some(ms);
        }
        chrono::DateTime::parse_from_rfc3339(raw.as_str()?)
            .ok()
            .map(|dt| dt.timestamp_millis())
    }

    fn role_of(&self, v: &Value) -> Option<Role> {
        let raw = match self.cfg.role_source {
            RoleSource::EntryType => at(v, &self.cfg.type_path)?.as_str()?,
            RoleSource::MessageRole => at(v, &self.cfg.role_path)?.as_str()?,
        };
        if self.cfg.user_roles.iter().any(|r| r == raw) {
            Some(Role::User)
        } else if self.cfg.assistant_roles.iter().any(|r| r == raw) {
            Some(Role::Assistant)
        } else {
            None
        }
    }

    /// Source identity/lineage/cost, by configured dotted path. Any path left
    /// unset yields `None` for that field.
    fn prov_of(&self, v: &Value) -> tp_core::turn::Provenance {
        let s = |p: &Option<String>| {
            p.as_deref()
                .and_then(|path| at(v, path))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        };
        let i = |p: &Option<String>| {
            p.as_deref()
                .and_then(|path| at(v, path))
                .and_then(|x| x.as_i64())
        };
        tp_core::turn::Provenance {
            uuid: s(&self.cfg.uuid_path),
            parent_uuid: s(&self.cfg.parent_uuid_path),
            // Absent reads as false: a runtime with no side-conversation
            // concept has none.
            sidechain: (!self.cfg.sidechain_path.is_empty())
                && at(v, &self.cfg.sidechain_path).and_then(|x| x.as_bool()) == Some(true),
            model: s(&self.cfg.model_path),
            cache_read_tokens: i(&self.cfg.cache_read_path),
            cache_creation_tokens: i(&self.cfg.cache_creation_path),
        }
    }

    /// Whether this record is a compaction boundary, the point after which
    /// everything before it stopped being context. Matched with the same
    /// `path == value` preconditions `require` uses, because a marker may be
    /// a subtype of a broader entry type.
    fn compaction_boundary(&self, v: &Value, seen: usize) -> Option<CompactionBoundary> {
        if self.cfg.compaction_markers.is_empty() {
            return None;
        }
        let matched = self
            .cfg
            .compaction_markers
            .iter()
            .all(|r| at(v, &r.path).and_then(|x| x.as_str()) == Some(r.equals.as_str()));
        if !matched {
            return None;
        }
        // Anchored when the descriptor names where the kept range starts: the
        // anchor may point earlier than the marker, so reading such a marker
        // positionally would report live context as superseded.
        if !self.cfg.compaction_keeps_from.is_empty() {
            return at(v, &self.cfg.compaction_keeps_from)
                .and_then(|x| x.as_str())
                // Declared but absent on this record: report nothing rather
                // than guess positionally.
                .map(|id| CompactionBoundary::Before(id.to_string()));
        }
        Some(CompactionBoundary::At(seen))
    }

    /// Readable reasoning text from a reasoning record, or empty. `block_type`
    /// empty means `text_path` is a plain string; otherwise it is an array of
    /// typed blocks and only matching ones contribute.
    fn reasoning_text(&self, v: &Value, rule: &ReasoningRule) -> String {
        if rule.text_path.is_empty() {
            return String::new();
        }
        let Some(node) = at(v, &rule.text_path) else {
            return String::new();
        };
        if let Some(s) = node.as_str() {
            return s.to_string();
        }
        let Some(arr) = node.as_array() else {
            return String::new();
        };
        let field = if self.cfg.text_field.is_empty() {
            "text"
        } else {
            &self.cfg.text_field
        };
        arr.iter()
            .filter(|b| {
                rule.block_type.is_empty()
                    || b.get("type").and_then(|t| t.as_str()) == Some(rule.block_type.as_str())
            })
            .filter_map(|b| b.get(field).and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The session title this record states, if it states one.
    fn title_of(&self, v: &Value) -> Option<(tp_core::turn::TitleSource, String)> {
        if self.cfg.title_rules.is_empty() {
            return None;
        }
        let ty = at(v, &self.cfg.type_path)?.as_str()?;
        let rule = self.cfg.title_rules.iter().find(|r| r.entry_type == ty)?;
        let title = at(v, &rule.title_path)?.as_str()?;
        Some((rule.title_source(), title.to_string()))
    }

    /// Text, thinking and tool blocks from one content array, wherever it
    /// lives. Shared so an `entry_rules` stanza with its own `content_path`
    /// reads a message exactly as the top-level path does.
    fn blocks_at(
        &self,
        v: &Value,
        content_path: &str,
    ) -> (Vec<String>, Vec<String>, Vec<ToolCallDigest>) {
        let mut text_blocks = Vec::new();
        let mut thinking_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        match at(v, content_path) {
            Some(Value::String(s)) => text_blocks.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    let Some(ty) = b.get("type").and_then(|t| t.as_str()) else {
                        continue;
                    };
                    if self.cfg.text_block.matches(ty) {
                        let field = if self.cfg.text_field.is_empty() {
                            ty
                        } else {
                            &self.cfg.text_field
                        };
                        if let Some(t) = b.get(field).and_then(|t| t.as_str()) {
                            text_blocks.push(t.to_string());
                        }
                    } else if self.cfg.thinking_block.matches(ty) {
                        let field = if self.cfg.thinking_field.is_empty() {
                            ty
                        } else {
                            &self.cfg.thinking_field
                        };
                        if let Some(t) = b.get(field).and_then(|t| t.as_str()) {
                            thinking_blocks.push(t.to_string());
                        }
                    } else if ty == self.cfg.tool_block {
                        let name = b
                            .get(&self.cfg.tool_name_field)
                            .and_then(|n| n.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let input_digest = b.get(&self.cfg.tool_input_field).map(digest_input);
                        tool_calls.push(ToolCallDigest { name, input_digest });
                    }
                }
            }
            _ => {}
        }
        (text_blocks, thinking_blocks, tool_calls)
    }

    fn parse_entry(&self, v: &Value) -> Option<NormalizedTurn> {
        // Preconditions first: a record that fails one is not conversational
        // content, whatever its type says.
        if !self
            .cfg
            .require
            .iter()
            .all(|r| at(v, &r.path).and_then(|x| x.as_str()) == Some(r.equals.as_str()))
        {
            return None;
        }
        // Entry-level rules win: these types carry text in a named field and
        // have no role of their own.
        if let Some(ty) = at(v, &self.cfg.type_path).and_then(|t| t.as_str()) {
            // Reasoning first: a reasoning record with no role would
            // otherwise fall through the role mapping and be dropped.
            if let Some(rule) = self.cfg.reasoning_rules.iter().find(|r| r.entry_type == ty) {
                let thinking = self.reasoning_text(v, rule);
                let opaque = !rule.opaque_path.is_empty()
                    && at(v, &rule.opaque_path).is_some_and(|x| !x.is_null());
                // Nothing readable and nothing opaque means no reasoning at
                // all: not a turn.
                if thinking.is_empty() && !opaque {
                    return None;
                }
                return Some(NormalizedTurn {
                    role: Role::Assistant,
                    ts: self.ts_of(v),
                    text: String::new(),
                    thinking,
                    thinking_opaque: opaque,
                    tool_calls: Vec::new(),
                    surface: Default::default(),
                    tokens_in: None,
                    tokens_out: None,
                    prov: self.prov_of(v),
                });
            }

            if let Some(rule) = self.cfg.entry_rules.iter().find(|r| r.entry_type == ty) {
                // `content_path` wins: a rule that names a block array gets the
                // full message parse, so thinking and tool calls survive.
                let (text, thinking, tool_calls) = if rule.content_path.is_empty() {
                    (
                        at(v, &rule.text_path)?.as_str()?.to_string(),
                        String::new(),
                        Vec::new(),
                    )
                } else {
                    let (t, th, tc) = self.blocks_at(v, &rule.content_path);
                    let (text, thinking) = (t.join(""), th.join(""));
                    // No text, no reasoning, no tool call: not a turn. An
                    // empty one would put a blank message in every window.
                    if text.is_empty() && thinking.is_empty() && tc.is_empty() {
                        return None;
                    }
                    (text, thinking, tc)
                };
                // Per-rule paths, falling back to the global ones; no shape
                // is hardcoded, so no runtime inherits another's by accident.
                let usage = |rule_path: &str, global: &str| {
                    let path = if rule_path.is_empty() {
                        global
                    } else {
                        rule_path
                    };
                    (!path.is_empty())
                        .then(|| at(v, path).and_then(|u| u.as_i64()))
                        .flatten()
                };
                return Some(NormalizedTurn {
                    role: if rule.role == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    ts: self.ts_of(v),
                    text,
                    thinking,
                    thinking_opaque: false,
                    tool_calls,
                    surface: Default::default(),
                    tokens_in: usage(&rule.usage_in_path, &self.cfg.usage_in_path),
                    tokens_out: usage(&rule.usage_out_path, &self.cfg.usage_out_path),
                    prov: self.prov_of(v),
                });
            }
        }
        let role = self.role_of(v)?;

        let (text_blocks, thinking_blocks, tool_calls) = self.blocks_at(v, &self.cfg.content_path);

        let mut text = text_blocks.join("");
        if text.is_empty() {
            for path in &self.cfg.text_fallbacks {
                if let Some(t) = at(v, path).and_then(|t| t.as_str()) {
                    text = t.to_string();
                    break;
                }
            }
        }
        if text.is_empty() {
            if let Some(j) = &self.cfg.text_join {
                // Gated on the first path existing, so this never invents text.
                if j.paths.first().and_then(|p| at(v, p)).is_some() {
                    let parts: Vec<String> = j
                        .paths
                        .iter()
                        .map(|p| at(v, p).and_then(|x| x.as_str()).unwrap_or("").to_string())
                        .collect();
                    text = format!("{}{}", j.prefix, parts.join(&j.sep));
                }
            }
        }

        Some(NormalizedTurn {
            role,
            ts: self.ts_of(v),
            text,
            prov: self.prov_of(v),
            thinking: thinking_blocks.join(""),
            thinking_opaque: false,
            tool_calls,
            surface: Default::default(),
            tokens_in: at(v, &self.cfg.usage_in_path).and_then(|u| u.as_i64()),
            tokens_out: at(v, &self.cfg.usage_out_path).and_then(|u| u.as_i64()),
        })
    }
}

impl Adapter for DeclAdapter {
    fn id(&self) -> &'static str {
        self.id
    }

    fn cwd_of_line(&self, line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        at(&v, &self.cfg.cwd_path)
            .and_then(|c| c.as_str())
            .map(str::to_string)
    }

    fn discover(&self, root: &Path) -> Result<Vec<SourceFile>> {
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        }
        for project_dir in dirs_at_depth(root, self.cfg.dir_depth, &self.cfg.nested_dir) {
            for entry in std::fs::read_dir(&project_dir)? {
                let entry = entry?;
                let path = entry.path();
                if !self.claims_file(&path) {
                    continue;
                }
                let Some(native_id) = self.native_id_from_path(&path) else {
                    continue;
                };
                let meta = entry.metadata()?;
                out.push(crate::adapter::source_file_at(path, &native_id, &meta));
            }
        }
        Ok(out)
    }

    fn locate(&self, root: &Path, native_id: &str) -> Result<Option<SourceFile>> {
        if !root.exists() || !crate::adapter::is_safe_native_id(native_id) {
            return Ok(None);
        }
        for project_dir in dirs_at_depth(root, self.cfg.dir_depth, &self.cfg.nested_dir) {
            // When the stem is the id the path can be addressed directly: one
            // stat per project dir instead of enumerating every session file.
            if self.cfg.native_id == NativeIdRule::Stem {
                let Some(sfx) = self.cfg.file_suffix.first() else {
                    continue;
                };
                let candidate = project_dir.join(format!("{native_id}{sfx}"));
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if meta.is_file() {
                        return Ok(Some(crate::adapter::source_file_at(
                            candidate, native_id, &meta,
                        )));
                    }
                }
                continue;
            }
            for entry in std::fs::read_dir(&project_dir)? {
                let entry = entry?;
                let path = entry.path();
                if !self.claims_file(&path) {
                    continue;
                }
                let hit = self
                    .native_id_from_path(&path)
                    .is_some_and(|id| id == native_id);
                if hit {
                    let meta = entry.metadata()?;
                    return Ok(Some(crate::adapter::source_file_at(path, native_id, &meta)));
                }
            }
        }
        Ok(None)
    }

    fn unsupported_reason(&self, first_line: &str) -> Option<String> {
        if self.cfg.version_path.is_empty() || self.cfg.supported_versions.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(first_line).ok()?;
        // A record that states no version is not a refusal: only the header
        // carries one.
        let found = at(&v, &self.cfg.version_path)?.as_i64()?;
        if self.cfg.supported_versions.contains(&found) {
            return None;
        }
        Some(format!(
            "{} transcript format version {found}, and this build reads {:?}",
            self.id, self.cfg.supported_versions
        ))
    }

    fn parse_line(&self, line: &str) -> Option<NormalizedTurn> {
        crate::adapter::jsonl::parse_one(line, |v| self.parse_entry(v))
    }

    fn title_of_line(&self, line: &str) -> Option<(tp_core::turn::TitleSource, String)> {
        if self.cfg.title_rules.is_empty() {
            return None;
        }
        self.title_of(&serde_json::from_str::<Value>(line).ok()?)
    }

    fn parse_from(&self, path: &Path, offset: u64) -> Result<ParseChunk> {
        // Bound before the call: an inline `Some(&closure)` would drop the
        // temporary while `read_chunk` still holds the reference.
        let boundary = |v: &Value, seen: usize| self.compaction_boundary(v, seen);
        crate::adapter::jsonl::read_chunk(
            path,
            offset,
            |v| {
                at(v, &self.cfg.cwd_path)
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
            },
            |v| self.parse_entry(v),
            |v| self.title_of(v),
            // `Some` only when the descriptor declares how to spot a boundary:
            // an adapter that cannot must report `unknown`, not `current`.
            if self.cfg.compaction_markers.is_empty() {
                None
            } else {
                Some(&boundary)
            },
        )
    }
}

#[cfg(test)]
mod tests {

    /// A codex-shaped descriptor exercises every vocabulary field that exists
    /// because a shape is not a rule: nested type path, an outer `require`,
    /// a nested role path, two block tags for one concept, a text field named
    /// differently from its block, and `cwd` under a payload.
    #[test]
    fn a_codex_shaped_descriptor_parses_codex_records() {
        let cfg: DeclConfig = toml::from_str(
            r#"
id = "codex"
native_id = "trailing_uuid"
dir_depth = 3
type_path = "payload.type"
cwd_path = "payload.cwd"
ts_path = "timestamp"
content_path = "payload.content"
role_source = "message_role"
role_path = "payload.role"
user_roles = ["user", "developer"]
assistant_roles = ["assistant"]
text_block = ["output_text", "input_text"]
text_field = "text"
[[require]]
path = "type"
equals = "response_item"
"#,
        )
        .expect("codex descriptor must parse");
        let a = DeclAdapter::new(cfg);

        let user = r#"{"timestamp":"2026-04-08T16:32:49.783Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"say hi"}]}}"#;
        let turn = a.parse_line(user).expect("a user message must parse");
        assert_eq!(turn.role, Role::User);
        assert_eq!(turn.text, "say hi");

        let asst = r#"{"timestamp":"2026-04-08T16:32:52.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}}"#;
        let turn = a.parse_line(asst).expect("an assistant message must parse");
        assert_eq!(turn.role, Role::Assistant);
        assert_eq!(turn.text, "hi there");

        // The load-bearing negative: this payload also has `type == "message"`
        // one level down, and without the outer requirement would be read as
        // conversation.
        let telemetry = r#"{"timestamp":"2026-04-08T16:32:50.000Z","type":"event_msg","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"NOT a turn"}]}}"#;
        assert!(
            a.parse_line(telemetry).is_none(),
            "a record failing `require` must not be read as conversation"
        );

        // cwd comes off the session_meta line through the adapter; a caller
        // reading a top-level `cwd` itself finds nothing here.
        let meta = r#"{"timestamp":"2026-04-08T16:32:49.747Z","type":"session_meta","payload":{"id":"019d6df0","cwd":"/Users/me/dev/proj"}}"#;
        assert_eq!(a.cwd_of_line(meta).as_deref(), Some("/Users/me/dev/proj"));
        assert_eq!(a.cwd_of_line(user), None);
    }

    /// The id is the trailing UUID of a stem that also carries a timestamp
    /// containing the same `-` separator, so no prefix split can find it.
    #[test]
    fn trailing_uuid_survives_a_dashed_timestamp() {
        assert_eq!(
            trailing_uuid("rollout-2026-04-08T09-32-49-019d6c6f-f1e8-7abc-afed-cba987654321"),
            Some("019d6c6f-f1e8-7abc-afed-cba987654321")
        );
        // Not a UUID shape: degrade rather than invent an id.
        assert_eq!(trailing_uuid("rollout-2026-04-08"), None);
        assert_eq!(trailing_uuid("plain"), None);
    }
    use super::*;

    /// Absolute expectations over the embedded claude_code descriptor: the
    /// descriptor is the implementation, so these pin the behaviour itself.
    #[test]
    fn the_embedded_claude_code_descriptor_reads_each_record_shape() {
        let decl = DeclAdapter::new(DeclConfig::claude_code());

        let t = decl
            .parse_line(r#"{"type":"user","cwd":"/Users/me/p","timestamp":"2026-08-03T23:30:43.911Z","message":{"content":"hello"}}"#)
            .unwrap();
        assert_eq!((t.role, t.text.as_str()), (Role::User, "hello"));

        let t = decl
            .parse_line(r#"{"type":"assistant","timestamp":"2026-08-03T23:30:44.000Z","message":{"content":[{"type":"thinking","thinking":"reasoning"},{"type":"text","text":"visible"},{"type":"tool_use","name":"read","input":{"path":"/tmp/x"}}],"usage":{"input_tokens":12,"output_tokens":34}}}"#)
            .unwrap();
        assert_eq!(t.role, Role::Assistant);
        assert_eq!(t.text, "visible");
        assert_eq!(t.thinking, "reasoning");
        assert_eq!((t.tokens_in, t.tokens_out), (Some(12), Some(34)));
        assert_eq!(t.tool_calls.len(), 1);
        assert_eq!(t.tool_calls[0].name, "read");

        // Non-conversational, malformed, blank: all skipped, never an error.
        for line in [
            r#"{"type":"queue-operation","timestamp":"2026-08-03T23:30:45.000Z"}"#,
            r#"{not json"#,
            "",
        ] {
            assert!(decl.parse_line(line).is_none(), "{line}");
        }
    }

    /// `parse_from`'s torn-line contract: an unterminated final line's bytes
    /// stay out of the offset, or the next poll silently drops that turn.
    #[test]
    fn parse_from_holds_back_the_torn_final_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sess.jsonl");
        let complete = "{\"type\":\"user\",\"cwd\":\"/Users/me/p\",\"timestamp\":\"2026-08-03T23:30:43.911Z\",\"message\":{\"content\":\"first\"}}\n";
        std::fs::write(&p, format!("{complete}{{\"type\":\"us")).unwrap();

        let chunk = DeclAdapter::new(DeclConfig::claude_code())
            .parse_from(&p, 0)
            .unwrap();
        assert_eq!(chunk.new_offset, complete.len() as u64);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.meta.cwd.as_deref(), Some("/Users/me/p"));
        assert_eq!(chunk.meta.title_derived.as_deref(), Some("first"));
    }

    #[test]
    fn native_id_rules_cover_both_shipped_layouts() {
        let cc = DeclAdapter::new(DeclConfig::claude_code());
        assert_eq!(cc.native_id_from_stem("uuid-1").unwrap(), "uuid-1");

        let mut pi_cfg = DeclConfig::claude_code();
        pi_cfg.native_id = NativeIdRule::AfterUnderscore;
        let pi = DeclAdapter::new(pi_cfg);
        assert_eq!(
            pi.native_id_from_stem("2026-08-03T19-46-39-778Z_0e2e0001")
                .unwrap(),
            "0e2e0001"
        );
        assert_eq!(
            pi.native_id_from_stem("no-separator").unwrap(),
            "no-separator"
        );
    }
}

#[cfg(test)]
mod pi_behavior {
    use super::*;

    /// Absolute expectations over the embedded pi descriptor: role at
    /// `message.role`, camelCase blocks, `input`/`output` usage, and entry
    /// types that carry text in a named field with no role of their own.
    #[test]
    fn the_embedded_pi_descriptor_reads_each_record_shape() {
        let decl = DeclAdapter::new(DeclConfig::pi());
        // (role, text, thinking, tokens_in, tokens_out, tool-call count), or
        // None for a record that is not conversation.
        type Expected = Option<(
            Role,
            &'static str,
            &'static str,
            Option<i64>,
            Option<i64>,
            usize,
        )>;
        let expect: &[(&str, Expected)] = &[
            (
                r#"{"type":"message","timestamp":"2026-08-03T23:30:43.911Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
                Some((Role::User, "hello", "", None, None, 0)),
            ),
            (
                r#"{"type":"message","timestamp":"2026-08-03T23:30:43.911Z","message":{"role":"user","content":"bare"}}"#,
                Some((Role::User, "bare", "", None, None, 0)),
            ),
            (
                r#"{"type":"message","timestamp":"2026-08-03T23:30:44.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret reasoning","thinkingSignature":"{\"id\":\"rs_0217\"}"},{"type":"text","text":"visible"}],"usage":{"input":12,"output":34}}}"#,
                Some((
                    Role::Assistant,
                    "visible",
                    "secret reasoning",
                    Some(12),
                    Some(34),
                    0,
                )),
            ),
            (
                r#"{"type":"message","timestamp":"2026-08-03T23:30:45.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"/tmp/x"}}]}}"#,
                Some((Role::Assistant, "", "", None, None, 1)),
            ),
            // toolResult maps to User to mirror Claude Code, where tool results
            // arrive inside `type:"user"` records.
            (
                r#"{"type":"message","timestamp":"2026-08-03T23:30:46.000Z","message":{"role":"toolResult","toolName":"read","content":[{"type":"text","text":"file body"}],"isError":false}}"#,
                Some((Role::User, "file body", "", None, None, 0)),
            ),
            // A compaction summary is indexed as a turn on purpose: it is the
            // distilled content of what pi no longer replays.
            (
                r#"{"type":"compaction","timestamp":"2026-08-03T23:30:48.000Z","summary":"discussed billing","tokensBefore":50000,"usage":{"input":7,"output":8}}"#,
                Some((
                    Role::Assistant,
                    "discussed billing",
                    "",
                    Some(7),
                    Some(8),
                    0,
                )),
            ),
            (
                r#"{"type":"custom_message","timestamp":"2026-08-03T23:30:49.000Z","customType":"floonet","content":"[floonet inbox] hi","display":true}"#,
                Some((Role::User, "[floonet inbox] hi", "", None, None, 0)),
            ),
            // Header, model change, and `custom` (pi's spec: never reaches the
            // model): none of these are conversation.
            (
                r#"{"type":"session","version":3,"id":"u","timestamp":"2026-08-03T23:30:43.872Z","cwd":"/Users/me/dev"}"#,
                None,
            ),
            (
                r#"{"type":"model_change","timestamp":"2026-08-03T23:30:43.9Z","provider":"openai","modelId":"gpt-4o"}"#,
                None,
            ),
            (
                r#"{"type":"custom","timestamp":"2026-08-03T23:30:43.9Z","customType":"x","data":{"n":1}}"#,
                None,
            ),
        ];
        for (line, want) in expect {
            let got = decl.parse_line(line);
            match (got, want) {
                (None, None) => {}
                (Some(t), Some((role, text, think, tin, tout, tools))) => {
                    assert_eq!(
                        (
                            t.role,
                            t.text.as_str(),
                            t.thinking.as_str(),
                            t.tokens_in,
                            t.tokens_out,
                            t.tool_calls.len()
                        ),
                        (*role, *text, *think, *tin, *tout, *tools),
                        "{line}"
                    );
                }
                (got, want) => panic!(
                    "{line}
  got {got:?}
  want {want:?}"
                ),
            }
        }
    }

    /// Lineage is read the way the engine reads everything else: a present id
    /// is an edge, a JSON `null` is no edge. pi writes `null` on records with
    /// no parent, and storing the null as a name would be an invented edge.
    #[test]
    fn the_pi_descriptor_reads_parentage_and_null_is_no_edge() {
        let d = DeclAdapter::new(DeclConfig::pi());
        let child = r#"{"type":"message","id":"9f53a34a","parentId":"317c9eee","timestamp":"2026-08-21T21:08:54.955Z","message":{"role":"user","content":"next"}}"#;
        let t = d.parse_line(child).unwrap();
        assert_eq!(
            (t.prov.uuid.as_deref(), t.prov.parent_uuid.as_deref()),
            (Some("9f53a34a"), Some("317c9eee")),
        );

        let root = r#"{"type":"message","id":"317c9eee","parentId":null,"timestamp":"2026-08-21T21:08:00.000Z","message":{"role":"user","content":"first"}}"#;
        let t = d.parse_line(root).unwrap();
        assert_eq!(
            (t.prov.uuid.as_deref(), t.prov.parent_uuid.as_deref()),
            (Some("317c9eee"), None),
            "a null parentId must not become an edge called \"null\"",
        );
    }

    /// `bashExecution` keeps no `content`; its text is composed
    /// (`$ <command>\n<output>`) via the descriptor's `text_join` rule.
    #[test]
    fn bash_execution_composes_command_and_output() {
        let line = r#"{"type":"message","timestamp":"2026-08-03T23:30:47.000Z","message":{"role":"bashExecution","command":"ls -la","output":"total 0","exitCode":0}}"#;
        let t = DeclAdapter::new(DeclConfig::pi()).parse_line(line).unwrap();
        assert_eq!(t.role, Role::User);
        assert_eq!(t.text, "$ ls -la\ntotal 0");
    }
}

#[cfg(test)]
mod title_rule_tests {
    use super::*;
    use tp_core::turn::TitleSource;

    use crate::adapter::decl::shipped_config as shipped;

    /// The shipped descriptor must carry the rules, not only the engine
    /// support them: a test that built its own config would pass while the
    /// shipped one stayed silent.
    #[test]
    fn the_shipped_claude_code_descriptor_reads_both_native_titles() {
        let cfg = shipped("claude_code");
        let a = DeclAdapter::new(cfg);

        // The field name is the one Claude Code writes; the values are synthetic.
        let ai = serde_json::json!({
            "type": "ai-title",
            "aiTitle": "Refactoring the parser",
            "sessionId": "e2e0a11c-0000-4000-8000-000000000002"
        });
        assert_eq!(
            a.title_of(&ai),
            Some((TitleSource::Ai, "Refactoring the parser".into()))
        );

        let custom = serde_json::json!({
            "type": "custom-title", "customTitle": "my own title", "sessionId": "x"
        });
        assert_eq!(
            a.title_of(&custom),
            Some((TitleSource::User, "my own title".into())),
            "a /rename must outrank the AI title on read"
        );

        // An ordinary message states no title, and must not be mistaken for one.
        let msg = serde_json::json!({
            "type": "user", "message": {"role": "user", "content": "hello"}
        });
        assert_eq!(a.title_of(&msg), None);
    }

    /// A title entry is not a turn. If it were also parsed as one, a rename
    /// would appear in search results as a message the user never sent.
    #[test]
    fn a_title_entry_contributes_no_turn() {
        let a = DeclAdapter::new(shipped("claude_code"));
        let ai = serde_json::json!({"type": "ai-title", "aiTitle": "t", "sessionId": "x"});
        assert!(a.parse_entry(&ai).is_none());
    }

    /// pi's native name, in the full record shape: the rule matches on
    /// `type_path`, and a shape assumed rather than copied is how a field
    /// name goes unnoticed.
    #[test]
    fn the_shipped_pi_descriptor_reads_its_session_name() {
        let a = DeclAdapter::new(shipped("pi"));
        let info = serde_json::json!({
            "type": "session_info", "id": "ea2f1441", "parentId": null,
            "timestamp": "2026-08-14T17:17:21.439Z", "name": "a-name-the-user-typed"
        });
        assert_eq!(
            a.title_of(&info),
            Some((TitleSource::User, "a-name-the-user-typed".into())),
            "pi has no model-generated name, so this is always a user title"
        );

        // pi's own `session` header is not a title, and carries no name.
        let header = serde_json::json!({
            "type": "session", "version": "3", "id": "019fc929", "cwd": "/w"
        });
        assert_eq!(a.title_of(&header), None);
    }

    /// A descriptor with no `title_rules` must report nothing rather than
    /// borrow another runtime's shape.
    #[test]
    fn a_descriptor_without_title_rules_finds_none() {
        let mut cfg = shipped("pi");
        cfg.title_rules.clear();
        let a = DeclAdapter::new(cfg);
        let info = serde_json::json!({"type": "session_info", "name": "my session"});
        assert_eq!(a.title_of(&info), None);
    }

    /// The precedence is not this file's to choose: `SessionMeta` owns it, and
    /// a later statement replaces an earlier one within a source because that
    /// is what a rename is.
    #[test]
    fn a_later_rename_replaces_an_earlier_one_but_not_across_sources() {
        let mut meta = tp_core::turn::SessionMeta::default();
        meta.set_title(TitleSource::Ai, "generated");
        meta.set_title(TitleSource::User, "chosen");
        meta.set_title(TitleSource::User, "chosen again");
        assert_eq!(meta.title_user.as_deref(), Some("chosen again"));
        assert_eq!(
            meta.title_ai.as_deref(),
            Some("generated"),
            "a user title must not erase the AI one — resolution happens on read"
        );
        assert!(meta.has_native_title());
    }

    /// A runtime may clear a name by setting it to whitespace, but the store
    /// cannot propagate the clear: a `None` in a chunk is indistinguishable
    /// from "no title here". An empty statement therefore records nothing,
    /// and the earlier title stays.
    #[test]
    fn an_empty_title_records_nothing_and_the_earlier_title_stays() {
        let mut meta = tp_core::turn::SessionMeta::default();
        meta.set_title(TitleSource::User, "named");
        meta.set_title(TitleSource::User, "   ");
        assert_eq!(
            meta.title_user.as_deref(),
            Some("named"),
            "the clear cannot cross the chunk boundary; the stored title survives"
        );
        assert!(meta.has_native_title());
    }
}

#[cfg(test)]
mod reasoning_rule_tests {
    use super::*;

    fn codex() -> DeclAdapter {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/codex.toml");
        DeclAdapter::new(toml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
    }

    /// codex emits reasoning as its own `response_item` with no `payload.role`;
    /// the role mapping alone would drop it, and storing "" would assert that
    /// no reasoning happened. The record has the real shape; the ids and
    /// payload are synthetic.
    #[test]
    fn an_encrypted_reasoning_record_is_a_turn_marked_opaque() {
        let v = serde_json::json!({
            "timestamp": "2026-08-16T03:29:55.668Z", "ordinal": 3, "type": "response_item",
            "payload": {
                "type": "reasoning", "id": "rs_0e2e000000000001",
                "summary": [],
                "encrypted_content": "gAAAAAB-synthetic-opaque-payload-not-a-real-blob",
                "internal_chat_message_metadata_passthrough": {"turn_id": "0e2e0002"}
            }
        });
        let t = codex()
            .parse_entry(&v)
            .expect("a reasoning record IS a turn");
        assert!(t.thinking_opaque, "reasoning happened and cannot be read");
        assert_eq!(t.thinking, "", "there is no readable text to store");
        assert_eq!(t.text, "", "reasoning is not message content");
        assert_eq!(t.role, Role::Assistant, "a user does not reason");
        assert_eq!(
            t.prov.uuid.as_deref(),
            Some("rs_0e2e000000000001"),
            "the record's own id is its address — the shipped descriptor maps payload.id"
        );
    }

    /// The shipped descriptor captures `payload.id` as the turn's address on
    /// messages too, and degrades to `None`, not an invented id, on rollouts
    /// whose message payloads predate the field.
    #[test]
    fn the_shipped_codex_descriptor_captures_the_payload_id() {
        let with_id = serde_json::json!({
            "timestamp": "2026-08-24T23:20:46.000Z", "type": "response_item",
            "payload": {"type": "message", "role": "user",
                        "id": "msg_01a03613-8cb0-7abc-afed-cba987654321",
                        "content": [{"type": "input_text", "text": "hi"}]}
        });
        let t = codex().parse_entry(&with_id).unwrap();
        assert_eq!(
            t.prov.uuid.as_deref(),
            Some("msg_01a03613-8cb0-7abc-afed-cba987654321"),
        );

        let predate = serde_json::json!({
            "timestamp": "2026-04-08T09:32:49.783Z", "type": "response_item",
            "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}]}
        });
        let t = codex().parse_entry(&predate).unwrap();
        assert_eq!(
            t.prov.uuid, None,
            "absent reads as no address, never a guess"
        );
    }

    /// When the model does produce a summary, that text is the reasoning and
    /// the record is not opaque. Built from codex's type declaration for the
    /// summary block rather than from an observed record.
    #[test]
    fn a_summarised_reasoning_record_carries_its_text() {
        let v = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning", "id": "rs_1",
                "summary": [
                    {"type": "summary_text", "text": "first I check the schema"},
                    {"type": "summary_text", "text": "then the migration"}
                ]
            }
        });
        let t = codex().parse_entry(&v).unwrap();
        assert_eq!(t.thinking, "first I check the schema\nthen the migration");
        assert!(!t.thinking_opaque, "readable text is not opaque");
    }

    /// Both present: partly readable. `opaque` must still win, because calling it
    /// plain `text` would overstate what floonet holds.
    #[test]
    fn a_summary_alongside_an_encrypted_payload_is_still_opaque() {
        let v = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning", "id": "rs_2",
                "summary": [{"type": "summary_text", "text": "a partial note"}],
                "encrypted_content": "gAAAA"
            }
        });
        let t = codex().parse_entry(&v).unwrap();
        assert_eq!(t.thinking, "a partial note", "keep what is readable");
        assert!(t.thinking_opaque, "and still say the rest is not");
    }

    /// A reasoning record with neither is not a turn; an empty one would
    /// inflate turn counts with rows carrying nothing.
    #[test]
    fn an_empty_reasoning_record_is_not_a_turn() {
        let v = serde_json::json!({
            "type": "response_item",
            "payload": {"type": "reasoning", "id": "rs_3", "summary": []}
        });
        assert!(codex().parse_entry(&v).is_none());
    }

    /// Reasoning is matched before the role mapping, so an ordinary codex
    /// message must still parse as one.
    #[test]
    fn an_ordinary_message_is_unaffected() {
        let v = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "say hi"}]
            }
        });
        let t = codex().parse_entry(&v).unwrap();
        assert_eq!(t.text, "say hi");
        assert_eq!(t.role, Role::User);
        assert!(!t.thinking_opaque);
    }

    /// A descriptor with no reasoning rules must not borrow codex's shape.
    #[test]
    fn a_runtime_without_reasoning_rules_is_unchanged() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/pi.toml");
        let pi = DeclAdapter::new(toml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap());
        let v = serde_json::json!({
            "type": "reasoning", "summary": [], "encrypted_content": "x"
        });
        assert!(pi.parse_entry(&v).is_none());
    }
}

#[cfg(test)]
mod compaction_rules {
    use super::*;
    use crate::adapter::decl::shipped_config;
    use serde_json::json;

    /// codex's compaction marker fails its own `[[require]]` gate (the line is
    /// `type: "compacted"`, not `response_item`). The boundary must still be
    /// reported, which holds only because `read_chunk` asks about compaction
    /// before `parse_entry` applies `require`. Driven through `parse_from`
    /// because the ordering is the whole claim.
    #[test]
    fn a_compaction_marker_that_fails_require_is_still_a_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("rollout-x.jsonl");
        let msg = |role: &str, text: &str| {
            format!(
                r#"{{"timestamp":"2026-08-17T00:00:00.000Z","type":"response_item","payload":{{"type":"message","role":"{role}","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
            )
        };
        std::fs::write(
            &f,
            format!(
                "{}
{}
{}
",
                msg("user", "before"),
                r#"{"timestamp":"2026-08-17T00:00:01.000Z","type":"compacted","payload":{"message":"summary of the dropped prefix"}}"#,
                msg("user", "after"),
            ),
        )
        .unwrap();

        let chunk = DeclAdapter::new(shipped_config("codex"))
            .parse_from(&f, 0)
            .unwrap();
        assert_eq!(
            chunk.turns.len(),
            2,
            "the marker is not conversation — `require` must reject it as a turn"
        );
        assert_eq!(
            chunk.compaction,
            vec![CompactionBoundary::At(1)],
            "…and it must STILL report a boundary, after the 1 turn seen before it"
        );
    }

    /// pi's boundary is anchored, and the shipped file must say so as a root
    /// key: a bare TOML key after a table header belongs to that table, and
    /// placed beside `[[compaction_markers]]` it would degrade pi to positional.
    #[test]
    fn the_shipped_pi_descriptor_anchors_its_boundary() {
        let cfg = shipped_config("pi");
        assert_eq!(
            cfg.compaction_keeps_from, "firstKeptEntryId",
            "must parse as a ROOT key, not as a field of some table above it"
        );
        let d = DeclAdapter::new(cfg);
        let line = json!({
            "type": "compaction",
            "id": "cmp-1",
            "firstKeptEntryId": "keep-me",
            "summary": "distilled",
            "timestamp": "2026-08-17T00:00:00.000Z"
        });
        assert_eq!(
            d.compaction_boundary(&line, 99),
            Some(CompactionBoundary::Before("keep-me".into())),
            "anchored, not At(99)"
        );
    }

    /// A marker whose anchor field is missing reports no boundary rather than
    /// falling back to its position: a positional guess would mark live
    /// context superseded, the one direction worth refusing to be wrong in.
    #[test]
    fn an_anchored_runtime_with_no_anchor_reports_nothing() {
        let d = DeclAdapter::new(shipped_config("pi"));
        let line = json!({"type": "compaction", "id": "cmp-1", "summary": "x"});
        assert_eq!(d.compaction_boundary(&line, 99), None);
    }
}

#[cfg(test)]
mod discover_parity {
    use super::*;
    use crate::adapter::decl::shipped_config as shipped_cfg;

    /// Which files the shipped claude_code descriptor discovers: a normal
    /// session beside its project, a subagent transcript under
    /// `<parent>/subagents/`, and a nested subagent one level deeper again.
    #[test]
    fn discover_agrees_with_the_shipped_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let proj = root.join("-Users-me-dev-thing");
        let sub = proj.join("parent-uuid").join("subagents");
        let nested = sub.join("agent-outer").join("subagents");
        std::fs::create_dir_all(&nested).unwrap();
        for (d, f) in [
            (&proj, "11111111-2222-3333-4444-555555555555.jsonl"),
            (&sub, "agent-aaaa.jsonl"),
            (&nested, "agent-bbbb.jsonl"),
        ] {
            std::fs::write(d.join(f), "{}\n").unwrap();
        }
        // Not transcripts. A workflow journal under `subagents/workflows/wf_*/`
        // is JSONL whose parent directory is `wf_...`, not `subagents`; a walk
        // that recursed into everything would collapse every journal onto one
        // bogus session.
        std::fs::write(proj.join("notes.txt"), "x").unwrap();
        let wf = sub.join("workflows").join("wf_abc123");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("journal.jsonl"), "{}\n").unwrap();

        let adapter = DeclAdapter::new(shipped_cfg("claude_code"));
        let mut a: Vec<String> = adapter
            .discover(root)
            .unwrap()
            .into_iter()
            .map(|s| s.native_id)
            .collect();
        a.sort();
        assert_eq!(
            a,
            vec![
                "11111111-2222-3333-4444-555555555555",
                "agent-aaaa",
                "agent-bbbb"
            ],
            "a subagent transcript, and a subagent OF a subagent, must both be found"
        );

        // `locate` walks the same tree: if it disagreed with `discover`, the
        // scan provider could not read a session the index held.
        for id in [
            "11111111-2222-3333-4444-555555555555",
            "agent-aaaa",
            "agent-bbbb",
        ] {
            let via_locate = adapter.locate(root, id).unwrap();
            assert!(via_locate.is_some(), "locate must see {id}");
            let via_discover = adapter
                .discover(root)
                .unwrap()
                .into_iter()
                .find(|s| s.native_id == id)
                .unwrap();
            assert_eq!(via_locate.unwrap().path, via_discover.path);
        }
        assert!(
            adapter.locate(root, "no-such-session").unwrap().is_none(),
            "a miss stays a miss"
        );
    }
}
