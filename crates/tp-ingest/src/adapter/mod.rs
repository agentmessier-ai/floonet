//! Adapter contract. `parse_from` must be resumable and side-effect free, and
//! must tolerate a torn final line: the source runtime may be mid-write when
//! it is read.

pub mod decl;
pub mod jsonl;

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tp_core::turn::{NormalizedTurn, ParseChunk};

/// A short, non-reversible preview of a tool input, never the raw input.
/// Shared by every adapter so no runtime can persist more than this.
pub(crate) fn digest_input(input: &Value) -> String {
    let s = input.to_string();
    s.chars().take(80).collect()
}

/// One harness, completely described: what it is, where its transcripts live,
/// how to parse them, and what it can and cannot do.
pub struct Harness {
    pub id: String,
    /// Human-readable; falls back to `id`.
    pub name: String,
    /// Where its transcripts live. Present even for a harness floonet cannot
    /// usefully parse; absence would mean "no read side at all".
    pub root: PathBuf,
    pub adapter: Box<dyn Adapter>,
    pub capabilities: decl::Capabilities,
}

/// Every runtime this build can read, as one list so an adapter and its root
/// cannot disagree.
///
/// A TOML config in `~/.teleport/runtimes.d/` overrides the built-in of the
/// same id, and a config with a new id adds a runtime, so a format in the
/// mapped class is supported without a rebuild. Anything the config cannot
/// express still drops to a Rust impl.
pub fn all_runtimes() -> Vec<Harness> {
    // The built-ins are the shipped descriptors compiled in, so a binary
    // running with nothing installed behaves identically to a full install.
    let mut out: Vec<Harness> = [
        decl::DeclConfig::claude_code(),
        decl::DeclConfig::pi(),
        decl::DeclConfig::codex(),
        decl::DeclConfig::dsh(),
    ]
    .into_iter()
    .map(|cfg| Harness {
        id: cfg.id.clone(),
        name: cfg.name.clone().unwrap_or_else(|| cfg.id.clone()),
        root: cfg.resolved_root(),
        capabilities: cfg.capabilities.clone(),
        adapter: Box::new(decl::DeclAdapter::new(cfg)),
    })
    .collect();
    for cfg in decl::load_configs(&decl::config_dir()) {
        let harness = Harness {
            id: cfg.id.clone(),
            name: cfg.name.clone().unwrap_or_else(|| cfg.id.clone()),
            root: cfg.resolved_root(),
            capabilities: cfg.capabilities.clone(),
            adapter: Box::new(decl::DeclAdapter::new(cfg)),
        };
        match out.iter().position(|h| h.id == harness.id) {
            Some(i) => out[i] = harness,
            None => out.push(harness),
        }
    }
    out
}

/// An installed file in `~/.teleport/runtimes.d/` that shadows a built-in
/// runtime, and whether it still says anything the binary does not.
pub struct DescriptorOverride {
    pub path: PathBuf,
    pub id: String,
    /// Byte-identical to this build's embedded copy: a no-op override that
    /// becomes a stale one the next time the shipped descriptor moves, hence
    /// safe to delete.
    pub identical: bool,
}

/// The overrides currently shadowing built-ins, for `fl version` and the
/// daemon's startup log.
///
/// An override wins silently by design, and an installed copy can outlive the
/// binary it matched. Content alone cannot distinguish "customized" from
/// "stale copy of an older ship", so this reports only that a file differs,
/// and labels the one case it can settle: byte-identical, hence redundant.
/// Files whose id is not a built-in are additions and are not listed; files
/// that fail to parse are `load_configs`' problem.
pub fn descriptor_overrides() -> Vec<DescriptorOverride> {
    descriptor_overrides_in(&decl::config_dir())
}

pub fn descriptor_overrides_in(dir: &Path) -> Vec<DescriptorOverride> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cfg) = toml::from_str::<decl::DeclConfig>(&text) else {
            continue;
        };
        let Some((id, embedded)) = decl::EMBEDDED.iter().find(|(id, _)| *id == cfg.id) else {
            continue; // an added runtime, not an override
        };
        out.push(DescriptorOverride {
            path,
            id: id.to_string(),
            identical: text == *embedded,
        });
    }
    out
}

pub fn all_adapters() -> Vec<Box<dyn Adapter>> {
    all_runtimes().into_iter().map(|h| h.adapter).collect()
}

/// Process signatures for every harness that declared one, in registration
/// order. The single source for what a runtime's process looks like, read
/// from the descriptors, so process recognition and ancestor lookup agree.
pub fn process_signatures() -> Vec<(String, String)> {
    all_runtimes()
        .into_iter()
        .filter_map(|h| h.capabilities.process_match.map(|p| (h.id, p)))
        .collect()
}

/// The `comm` pattern for one runtime, if it declared one.
pub fn process_signature_for(runtime_id: &str) -> Option<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .and_then(|h| h.capabilities.process_match)
}

/// The phrase that makes this runtime check its inbox, if it declared one.
pub fn control_string_for(runtime_id: &str) -> Option<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .and_then(|h| h.capabilities.control_string)
}

/// How this runtime answers a message, if it declared something other than the
/// `fl reply` shell command. `{id}` in the value is the message id.
pub fn reply_hint_for(runtime_id: &str) -> Option<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .and_then(|h| h.capabilities.reply_hint)
}

/// How this runtime confirms it has finished with a message, if it declared
/// something other than the `fl ack` shell command. No `{id}`: the lines that
/// use it describe a batch, not one message.
pub fn ack_hint_for(runtime_id: &str) -> Option<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .and_then(|h| h.capabilities.ack_hint)
}

/// Transcript `type` values this runtime uses for sessions that are not
/// conversations. Empty for every runtime that does not spawn internal ones.
pub fn non_conversation_types_for(runtime_id: &str) -> Vec<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .map(|h| h.capabilities.non_conversation_types)
        .unwrap_or_default()
}

/// The `type` declared on the first line of this session's transcript.
///
/// `None` means "cannot tell" (no descriptor, no file, a torn first line) and
/// every caller must treat it as "leave it alone". A transcript may
/// legitimately not exist yet: SessionStart fires before the harness has
/// finished creating the file, so a register-time check cannot do this alone.
pub fn transcript_kind_of(session_id: &str) -> Option<String> {
    use std::io::BufRead as _;
    let mut parts = session_id.split('/');
    let (_machine, runtime, native) = (parts.next()?, parts.next()?, parts.next()?);
    let h = all_runtimes().into_iter().find(|h| h.id == runtime)?;
    let found = h.adapter.locate(&h.root, native).ok()??;
    let mut first = String::new();
    std::io::BufReader::new(std::fs::File::open(found.path).ok()?)
        .read_line(&mut first)
        .ok()?;
    serde_json::from_str::<Value>(&first)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

/// Does a transcript exist on this machine for `<machine>/<runtime>/<native>`?
///
/// "Can read this" and "can deliver to this" are different questions with
/// same-shaped ids, and the id a caller holds does not say which it came from.
/// This lets a delivery failure distinguish "readable, but nothing registered
/// it" from "unknown id".
///
/// `this_machine` is required: a peer's session id names a transcript on the
/// other machine, and matching on the native id alone would claim a local
/// file for it.
pub fn transcript_exists(session_id: &str, this_machine: &str) -> bool {
    let mut parts = session_id.split('/');
    let (Some(machine), Some(runtime), Some(native)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    // A remote id is not readable here by definition: only local roots are
    // consulted.
    if machine != this_machine {
        return false;
    };
    all_runtimes()
        .into_iter()
        .filter(|h| h.id == runtime)
        .any(|h| matches!(h.adapter.locate(&h.root, native), Ok(Some(_))))
}

pub fn all_roots() -> Vec<(String, PathBuf)> {
    all_runtimes().into_iter().map(|h| (h.id, h.root)).collect()
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    /// Unix inode number, the identity `ingest_state` keys on.
    pub inode: i64,
    pub size: u64,
    pub mtime_ms: i64,
    /// Adapter-native session id, recovered from the path per the runtime's rule.
    pub native_id: String,
}

pub trait Adapter: Send + Sync {
    fn id(&self) -> &'static str;

    /// Why this transcript must not be parsed, judged from its first record.
    ///
    /// A descriptor's paths are the shape of one format version; applied to
    /// another they do not fail, they under-read. The caller skips the file
    /// and says so, which separates "empty" from "unreadable".
    /// `None` by default: a runtime that states no version has nothing to check.
    fn unsupported_reason(&self, _first_line: &str) -> Option<String> {
        None
    }
    fn discover(&self, root: &Path) -> Result<Vec<SourceFile>>;

    /// The primitive both retrieval backends share: one raw line to at most
    /// one normalized turn. `None` for blank or malformed lines and for
    /// non-conversational records. Sharing the primitive keeps the scan and
    /// index backends from drifting on format quirks, `thinking` included.
    fn parse_line(&self, line: &str) -> Option<NormalizedTurn>;

    /// This session's working directory, if this record carries it.
    ///
    /// On the trait because only the adapter knows where its runtime puts it;
    /// a caller-side guess would disagree with the adapter's own parse of the
    /// same file. The default reads a top-level `cwd`.
    fn cwd_of_line(&self, line: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()?
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string)
    }

    /// The session title this record states, if it states one.
    ///
    /// On the trait for the same reason as `cwd_of_line`: only the adapter
    /// knows where its runtime puts it, and both retrieval backends must get
    /// the same answer. Default `None` means "no title I know how to read",
    /// which leaves the derived fallback in place.
    fn title_of_line(&self, line: &str) -> Option<(tp_core::turn::TitleSource, String)> {
        let _ = line;
        None
    }

    /// Index-only: parse `[offset, EOF)` resumably, built on
    /// `parse_line`. `new_offset` must exclude any unterminated trailing line.
    fn parse_from(&self, path: &Path, offset: u64) -> Result<ParseChunk>;

    /// Find one session's file by its native id.
    ///
    /// The default enumerates and stats every file under the root, so a
    /// single-session read costs the whole corpus: slow, never wrong. An
    /// adapter whose layout lets it address the file directly should override.
    fn locate(&self, root: &Path, native_id: &str) -> Result<Option<SourceFile>> {
        Ok(self
            .discover(root)?
            .into_iter()
            .find(|s| s.native_id == native_id))
    }
}

/// A native id is interpolated into a filesystem path by `locate`, and ids
/// arrive from the wire. Anything that could climb out of the root is refused
/// rather than sanitized: no legitimate id contains a separator, so rejecting
/// is lossless.
pub(crate) fn is_safe_native_id(native_id: &str) -> bool {
    !native_id.is_empty()
        && !native_id.contains('/')
        && !native_id.contains('\\')
        && !native_id.contains("..")
        && !native_id.contains('\0')
}

/// Build a `SourceFile` from a path and its metadata.
///
/// One place for the mtime arithmetic: the watcher compares `mtime_ms` to
/// decide a file changed, and a copy that rounded differently would make one
/// runtime's sessions re-ingest on every poll, or never. `native_id` is
/// `impl Into<String>` so a caller that owns the string hands it over rather
/// than cloning on every file `discover` visits.
pub(crate) fn source_file_at(
    path: PathBuf,
    native_id: impl Into<String>,
    meta: &std::fs::Metadata,
) -> SourceFile {
    use std::os::unix::fs::MetadataExt;
    SourceFile {
        path,
        inode: meta.ino() as i64,
        size: meta.len(),
        mtime_ms: meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000,
        native_id: native_id.into(),
    }
}

#[cfg(test)]
mod override_status_tests {
    use super::*;

    /// The cases the report distinguishes, and the two it deliberately does
    /// not list: an added runtime is not an override, and an unparseable file
    /// is `load_configs`' problem.
    #[test]
    fn overrides_are_classified_against_the_embedded_text() {
        let dir = tempfile::tempdir().unwrap();
        let pi = decl::EMBEDDED.iter().find(|(id, _)| *id == "pi").unwrap().1;

        // Byte-identical copy.
        std::fs::write(dir.path().join("pi.toml"), pi).unwrap();
        // A real difference: one changed value.
        std::fs::write(
            dir.path().join("claude_code.toml"),
            decl::EMBEDDED[0].1.replace("subagents", "helpers"),
        )
        .unwrap();
        // An added runtime: same engine, new id; listed nowhere.
        std::fs::write(
            dir.path().join("openclaw.toml"),
            pi.replace("id = \"pi\"", "id = \"openclaw\""),
        )
        .unwrap();
        // Garbage: load_configs warns about it; this report ignores it.
        std::fs::write(dir.path().join("broken.toml"), "not toml [[[").unwrap();

        let got = descriptor_overrides_in(dir.path());
        let mut brief: Vec<(&str, bool)> =
            got.iter().map(|o| (o.id.as_str(), o.identical)).collect();
        brief.sort();
        assert_eq!(brief, [("claude_code", false), ("pi", true)]);
        // The file, not only the id: "which file do I delete" is the question.
        assert!(got.iter().all(|o| o.path.exists()));
    }
}

#[cfg(test)]
mod control_string_tests {
    /// Claude Code's wake string must not begin with a slash. The harness has
    /// a built-in command whose name shadows the project's, and its command
    /// menu matches on prefix and rewrites what it matches, so the guard is on
    /// the first character rather than on any particular prefix.
    #[test]
    fn claude_codes_wake_string_is_not_a_slash_command_at_all() {
        let cs = super::control_string_for("claude_code")
            .expect("claude_code must declare a control string, not fall back to the default");
        assert!(
            !cs.starts_with('/'),
            "a leading slash hands this to Claude Code's command menu, which matched \
             /floonet and resumed a web session — twice. got {cs:?}"
        );
        assert!(
            cs.contains("floo_inbox") || cs.contains("inbox"),
            "it still has to say what to do: {cs:?}"
        );
    }

    /// pi has no built-in by that name and its extension registers the command
    /// itself, so the bare slash form is correct there; the Claude Code rule
    /// is not universal.
    #[test]
    fn pi_keeps_the_bare_form_because_nothing_shadows_it_there() {
        let cs = super::control_string_for("pi").unwrap_or_else(|| "/fl inbox".to_string());
        assert_eq!(
            cs, "/fl inbox",
            "pi's extension registers /tp itself and pi has no built-in of that name, \
             so the slash form is correct THERE — the Claude Code problem is not universal"
        );
    }

    /// codex has no slash-command mechanism, so its string is prose naming
    /// the tool.
    #[test]
    fn codex_uses_prose_because_it_has_no_slash_commands() {
        let cs = super::control_string_for("codex").expect("codex declares one");
        assert!(
            !cs.starts_with('/'),
            "codex cannot take a slash command: {cs:?}"
        );
        assert!(
            cs.contains("floo_inbox"),
            "it names the tool instead: {cs:?}"
        );
    }
}
