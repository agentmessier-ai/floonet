//! The descriptor vocabulary: what a `runtimes.d/*.toml` file can say, the
//! shipped descriptors embedded in the binary, and how user files are loaded.
//! The engine that acts on these lives in `engine.rs`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// How a session's native id is recovered from its filename stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeIdRule {
    /// The stem is the id (`<uuid>.jsonl`).
    Stem,
    /// Everything after the first `_` (`<timestamp>_<uuid>.jsonl`).
    AfterUnderscore,
    /// The trailing UUID of a dash-separated stem (`rollout-<timestamp>-<uuid>`).
    /// No prefix split can find it: the timestamp uses the same `-` the UUID does.
    TrailingUuid,
    /// The containing directory's name (`session-<uuid>/session.jsonl.zstd`).
    /// When every file shares one name, no rule over the stem can tell two
    /// sessions apart.
    ParentDir,
}

/// Where a record's role comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// The entry's own type, at `type_path`.
    EntryType,
    /// A field on the nested message, at `role_path`.
    MessageRole,
}

/// One or several accepted values, so a descriptor can say "either of these"
/// without the config growing a parallel plural field.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub(crate) fn matches(&self, s: &str) -> bool {
        match self {
            OneOrMany::One(v) => v == s,
            OneOrMany::Many(vs) => vs.iter().any(|v| v == s),
        }
    }
}

impl Default for OneOrMany {
    fn default() -> Self {
        OneOrMany::One(String::new())
    }
}

/// One `path == value` precondition on a record.
///
/// `deny_unknown_fields` on this and the other rule structs: a bare TOML key
/// written after a table header belongs to that table, so a root-level setting
/// placed beside the rule it describes lands inside the rule. Rejecting it
/// makes the misplacement a load error rather than a silently dropped setting.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub path: String,
    pub equals: String,
}

fn default_type_path() -> String {
    "type".to_string()
}
fn default_role_path() -> String {
    "message.role".to_string()
}
fn default_cwd_path() -> String {
    "cwd".to_string()
}
fn default_file_suffix() -> Vec<String> {
    vec![".jsonl".to_string()]
}

fn default_dir_depth() -> usize {
    1
}

/// The trailing `8-4-4-4-12` UUID of a dash-separated stem, if there is one.
pub(crate) fn trailing_uuid(stem: &str) -> Option<&str> {
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let tail = &parts[parts.len() - 5..];
    let shaped = [8usize, 4, 4, 4, 12]
        .iter()
        .zip(tail)
        .all(|(want, got)| got.len() == *want && got.chars().all(|c| c.is_ascii_hexdigit()));
    if !shaped {
        return None;
    }
    let start = stem.len() - (tail.iter().map(|p| p.len()).sum::<usize>() + 4);
    Some(&stem[start..])
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryRule {
    pub entry_type: String,
    /// "user" | "assistant"
    pub role: String,
    /// Path to the text, relative to the record. For an entry whose payload is
    /// a plain string.
    #[serde(default)]
    pub text_path: String,
    /// Path to a content block array, parsed exactly as the top-level
    /// `content_path` is. For a runtime whose message shapes keep content at
    /// different paths: one global `content_path` can name only one of them,
    /// and `text_path` reads a string, which would drop reasoning and tool
    /// blocks. Wins over `text_path` when both are set.
    #[serde(default)]
    pub content_path: String,
    /// Per-rule token counts, for a runtime that reports usage at a path
    /// specific to the message shape. Empty falls back to the global paths.
    #[serde(default)]
    pub usage_in_path: String,
    #[serde(default)]
    pub usage_out_path: String,
}

/// An entry that names the session instead of contributing a turn.
///
/// User and AI titles are stored separately and resolved at read time, so a
/// rename arriving late wins without a rewrite. A rule matches on the same
/// `type_path` `entry_rules` uses, so a title entry costs a stanza rather
/// than a code change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleRule {
    pub entry_type: String,
    /// Path to the title text, relative to the record.
    pub title_path: String,
    /// `"user"` (a person named it) or `"ai"` (a model did). Decides read
    /// precedence, so it is required rather than defaulted: a wrong guess
    /// silently outranks a real title.
    pub source: String,
}

impl TitleRule {
    pub(crate) fn title_source(&self) -> tp_core::turn::TitleSource {
        match self.source.as_str() {
            "ai" => tp_core::turn::TitleSource::Ai,
            // Anything else is user-set, the safer side to err on: mistaking
            // `ai` for `user` shows a real title too prominently, while the
            // reverse would hide it.
            _ => tp_core::turn::TitleSource::User,
        }
    }
}

/// A record whose payload is reasoning, not a message: a runtime that emits
/// thinking as its own record with no role, which the role mapping would
/// otherwise drop, misreporting the turn as having done no reasoning.
///
/// Becomes a turn of its own with empty `text`. Merging it into the
/// neighbouring assistant message would need cross-line state, and the
/// adapter contract is per-line and stateless so a resumed read cannot
/// disagree with a full one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningRule {
    pub entry_type: String,
    /// Where readable reasoning lives, if the record has any. Blocks are read
    /// the same way `content_path` reads message content.
    #[serde(default)]
    pub text_path: String,
    /// Which block type inside `text_path` carries the text. Empty means the
    /// path holds a plain string.
    #[serde(default)]
    pub block_type: String,
    /// A path whose presence means reasoning happened but cannot be read.
    /// Distinct from `text_path` being absent, which means there was nothing.
    #[serde(default)]
    pub opaque_path: String,
}

/// Compose text from several paths, for shapes that keep no `content` at all
/// (a command and its output, rendered as `$ <command>\n<output>`). Applies
/// only when the first path is present, so it cannot manufacture text for
/// unrelated records.
#[derive(Debug, Clone, Deserialize)]
pub struct TextJoin {
    #[serde(default)]
    pub prefix: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub sep: String,
}

/// What a harness can and cannot do, so core stops assuming. Each field
/// removes one assumption, and the defaults describe a one-session-per-process
/// terminal harness, so a descriptor that omits the section is unchanged.
///
/// - `scannable`: the process scan may prune registrations it did not find.
///   A harness the scan cannot see would have correct rows pruned each cycle.
/// - `multiplexed`: one host process serves many sessions, so reconcile must
///   not collapse its registrations to one row per pid.
/// - `heartbeats`: the harness renews its own presence; one that cannot is
///   why the process scan exists.
/// - `pane`: there is a tty to type the control string into. Without one, a
///   declared delivery channel is required rather than optional.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Capabilities {
    pub scannable: bool,
    pub multiplexed: bool,
    pub heartbeats: bool,
    pub pane: bool,
    /// How a scannable harness is recognized in `ps` output: a substring match
    /// on the process's `comm`. Not a gate on whether a runtime may exist; a
    /// harness that registers itself needs no signature. `None` means "not
    /// discoverable without cooperation".
    pub process_match: Option<String>,
    /// The fixed phrase typed into this runtime's pane to make it drain its
    /// inbox. `None` uses the default `/fl inbox`. Declarable because it is
    /// the runtime's own vocabulary: a runtime with no such command needs
    /// prose instead.
    ///
    /// This does not weaken the pane invariant (nothing from a message crosses
    /// a pane): the value comes from a descriptor the operator controls, is
    /// read once, and is never interpolated from any request field.
    pub control_string: Option<String>,
    /// How this runtime answers a message. `None` uses `fl reply <id> "..."`.
    ///
    /// Per-runtime because the sandbox is: `fl reply` is a shell command, and
    /// a runtime that confines its shell may still run its integration
    /// unconfined. The wrong hint is not a clear error but a silent hang on a
    /// permission prompt nobody is watching, so the descriptor states it.
    pub reply_hint: Option<String>,
    /// How this runtime confirms it has finished with a message. `None` uses
    /// the default `` `fl ack <id>` ``.
    ///
    /// Separate from `reply_hint` rather than inferred from it: a runtime that
    /// cannot run one shell command cannot run the other, but deriving the
    /// second fact from the first couples them, and the day a runtime declares
    /// one without the other the wrong instruction ships silently.
    ///
    /// Carries no `{id}`: the lines that use it summarise a whole drained
    /// batch, so naming one message id would be a claim about which.
    pub ack_hint: Option<String>,
    /// Values of this runtime's transcript `type` field that mean "this is not
    /// a conversation": a session that must not register.
    ///
    /// A runtime may spawn short-lived headless sub-conversations that do real
    /// work but are not correspondents: nobody addresses one, and it is gone
    /// in seconds. Their rows would otherwise outlive them, because a hook row
    /// is removed when its process dies and the process is the long-lived
    /// pane. Declared per runtime because the vocabulary is the runtime's.
    #[serde(default)]
    pub non_conversation_types: Vec<String>,
}

impl Default for Capabilities {
    /// A descriptor that declares nothing: one session per process,
    /// scannable, pane-injectable, cannot heartbeat.
    fn default() -> Self {
        Self {
            scannable: true,
            multiplexed: false,
            heartbeats: false,
            pane: true,
            process_match: None,
            control_string: None,
            reply_hint: None,
            ack_hint: None,
            // Empty means every session is a conversation.
            non_conversation_types: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeclConfig {
    pub id: String,
    /// Transcript root. `~` is expanded. Omitted falls back to the built-in
    /// default for this id, so a config need only override what differs.
    #[serde(default)]
    pub root: Option<String>,
    pub native_id: NativeIdRule,
    pub role_source: RoleSource,
    /// Source role value → normalized role. Anything unlisted is not a
    /// conversational turn and is skipped, which is how non-message records
    /// (queue-operation, model_change, …) fall out without special cases.
    pub user_roles: Vec<String>,
    pub assistant_roles: Vec<String>,
    /// Dotted paths, resolved against the raw record.
    pub ts_path: String,
    pub content_path: String,
    #[serde(default)]
    pub usage_in_path: String,
    #[serde(default)]
    pub usage_out_path: String,
    /// Block `type` discriminators and the field each carries.
    pub text_block: OneOrMany,
    #[serde(default)]
    pub thinking_block: OneOrMany,
    #[serde(default)]
    pub tool_block: String,
    #[serde(default)]
    pub tool_name_field: String,
    #[serde(default)]
    pub tool_input_field: String,
    /// Entry types outside the normal role mapping (see `EntryRule`).
    #[serde(default)]
    pub entry_rules: Vec<EntryRule>,
    /// Entry types that are reasoning records rather than messages (see
    /// `ReasoningRule`).
    #[serde(default)]
    pub reasoning_rules: Vec<ReasoningRule>,
    /// How to recognise a compaction boundary: all of these `path == value`
    /// must hold. Empty means this runtime's marker is unknown, and its turns
    /// are recorded as `unknown` rather than `current`.
    #[serde(default)]
    pub compaction_markers: Vec<Requirement>,
    /// Path to the id of the first entry kept by a compaction, when the
    /// runtime names one. Empty means the marker's own position is the boundary.
    #[serde(default)]
    pub compaction_keeps_from: String,
    /// Entry types that carry a session title rather than a turn (see
    /// `TitleRule`).
    #[serde(default)]
    pub title_rules: Vec<TitleRule>,
    /// Tried in order when `content` yields no text, for message shapes that
    /// keep their payload elsewhere.
    #[serde(default)]
    pub text_fallbacks: Vec<String>,
    #[serde(default)]
    pub text_join: Option<TextJoin>,
    /// Dotted paths to source identity/lineage/cost. All optional: a format
    /// without a per-message id omits them and the turn keeps a `None`,
    /// `seq`-ordered `Provenance`. A field absent here is a field not captured.
    #[serde(default)]
    pub uuid_path: Option<String>,
    #[serde(default)]
    pub parent_uuid_path: Option<String>,
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default)]
    pub cache_read_path: Option<String>,
    #[serde(default)]
    pub cache_creation_path: Option<String>,
    /// Human-readable name for `fl live` / diagnostics. Falls back to `id`.
    #[serde(default)]
    pub name: Option<String>,
    /// Dotted path to the record's type, which `entry_rules` and
    /// `role_source = "entry_type"` match against. Defaults to top-level `type`.
    #[serde(default = "default_type_path")]
    pub type_path: String,
    /// Dotted path to the transcript's own format version, and the versions
    /// this descriptor was written against.
    ///
    /// Both empty (the default) means the runtime states no version and
    /// nothing is checked. Where a runtime does state one, a mismatch is
    /// refused rather than parsed on a guess: a descriptor's paths applied to
    /// another version report whatever falls out, presented as the whole.
    #[serde(default)]
    pub version_path: String,
    #[serde(default)]
    pub supported_versions: Vec<i64>,
    /// Field holding a text block's text. Empty (the default) means "the same
    /// name as the block's type", a common shape but not a rule.
    #[serde(default)]
    pub text_field: String,
    /// Same, for thinking blocks.
    #[serde(default)]
    pub thinking_field: String,
    /// Dotted path to the role, used when `role_source = "message_role"`.
    /// Defaults to `message.role`. Ignored for `role_source = "entry_type"`,
    /// which reads `type_path` instead.
    #[serde(default = "default_role_path")]
    pub role_path: String,
    /// Extra path/value equalities a record must satisfy to be considered at
    /// all, for a runtime whose `type_path` value is ambiguous on its own.
    #[serde(default)]
    pub require: Vec<Requirement>,

    /// Path to a boolean saying this record belongs to a side conversation.
    /// Empty means the runtime has no such concept.
    #[serde(default)]
    pub sidechain_path: String,
    /// Dotted path to the session's working directory. Defaults to a
    /// top-level `cwd` on any record.
    #[serde(default = "default_cwd_path")]
    pub cwd_path: String,
    /// How many directory levels sit between the runtime root and a
    /// transcript: 1 for `root/<project>/<file>`, more for a date-partitioned
    /// tree.
    #[serde(default = "default_dir_depth")]
    pub dir_depth: usize,
    /// The filename suffixes a transcript is recognised by, longest first.
    ///
    /// Not an extension: `session.jsonl.zstd` has extension `zstd`, and
    /// matching on that would claim any zstd file. A list because one runtime
    /// may write either encoding, and naming only one would report zero
    /// sessions, not an error, for the other.
    #[serde(default = "default_file_suffix")]
    pub file_suffix: Vec<String>,
    /// A directory name, at any depth, whose files are transcripts too: for a
    /// runtime that nests subagent transcripts below a session, recursively.
    ///
    /// A name rather than "recurse into everything": the nesting path passes
    /// through directories holding non-transcript JSONL, and matching the
    /// immediate parent separates them exactly. A separate opt-in rather than
    /// a minimum on `dir_depth`, which would change what other runtimes scan.
    #[serde(default)]
    pub nested_dir: String,
    /// What this harness can and cannot do; see `Capabilities`.
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// The shipped descriptors, byte for byte, keyed by runtime id. Exposed so the
/// override report and `fl version` can compare an installed override against
/// exactly what this build carries.
pub const EMBEDDED: [(&str, &str); 4] = [
    (
        "claude_code",
        include_str!("../../../../../install/runtimes.d/claude_code.toml"),
    ),
    (
        "pi",
        include_str!("../../../../../install/runtimes.d/pi.toml"),
    ),
    (
        "codex",
        include_str!("../../../../../install/runtimes.d/codex.toml"),
    ),
    (
        "dsh",
        include_str!("../../../../../install/runtimes.d/dsh.toml"),
    ),
];

impl DeclConfig {
    /// A shipped descriptor, compiled into the binary as its TOML text rather
    /// than restated in Rust, so there is one text per runtime and no second
    /// copy to drift.
    ///
    /// # Panics
    /// If the embedded text does not parse; it is fixed at compile time and
    /// covered by the crate's tests.
    fn embedded(text: &'static str) -> Self {
        toml::from_str(text)
            .expect("embedded descriptor must parse — see embedded_descriptors_parse")
    }

    pub fn claude_code() -> Self {
        Self::embedded(EMBEDDED[0].1)
    }

    pub fn pi() -> Self {
        Self::embedded(EMBEDDED[1].1)
    }

    pub fn dsh() -> Self {
        Self::embedded(EMBEDDED[3].1)
    }

    pub fn codex() -> Self {
        Self::embedded(EMBEDDED[2].1)
    }
}

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    Path::new(&home).join(".teleport").join("runtimes.d")
}

/// Expand `${VAR}` and `${VAR:-fallback}` against the environment, then `~`.
///
/// Runtimes let the user move their home with an environment variable, and a
/// descriptor that cannot follow it finds zero sessions rather than failing.
/// `~` expansion runs last so a fallback may itself be `~/...`.
///
/// An unset variable with no fallback expands to empty rather than the literal
/// `${VAR}`: an unexpanded placeholder reads as "this runtime has no sessions"
/// rather than "this descriptor asked for something absent". Empty makes the
/// root obviously wrong instead of plausibly empty.
fn expand_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find("${") {
        // The closing brace is checked before any prefix is committed, so
        // bailing out leaves the prefix for the push after the loop, once.
        let Some(close) = rest[i..].find('}') else {
            // No closing brace: not a placeholder, just text.
            break;
        };
        out.push_str(&rest[..i]);
        let inner = &rest[i + 2..i + close];
        let (name, fallback) = match inner.split_once(":-") {
            Some((n, f)) => (n, Some(f)),
            None => (inner, None),
        };
        let value = std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| fallback.map(str::to_string))
            .unwrap_or_default();
        out.push_str(&value);
        rest = &rest[i + close + 1..];
    }
    out.push_str(rest);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    // Only a leading `~` names a home. Elsewhere a tilde is an ordinary
    // character in an ordinary directory name, and `~user` is someone else's
    // home, which nothing here can resolve.
    match out.strip_prefix('~') {
        Some("") => home,
        Some(after) if after.starts_with('/') => format!("{home}{after}"),
        _ => out,
    }
}

impl DeclConfig {
    /// Resolved transcript root: the config's own, expanded, else the built-in
    /// default for this id.
    pub fn resolved_root(&self) -> PathBuf {
        match &self.root {
            Some(r) => PathBuf::from(expand_path(r)),
            None => default_root_for(&self.id),
        }
    }
}

#[cfg(test)]
mod expand_path_tests {
    use super::expand_path;

    /// One test: these mutate process-wide environment, and tests of one
    /// binary run on threads that share it.
    #[test]
    fn placeholders_resolve_against_the_environment() {
        // SAFETY: single test touching these names; no other test reads them.
        unsafe {
            std::env::set_var("TP_TEST_ROOT", "/somewhere/else");
            std::env::remove_var("TP_TEST_UNSET");
        }
        let home = std::env::var("HOME").unwrap();

        // Set wins over the fallback.
        assert_eq!(
            expand_path("${TP_TEST_ROOT:-~/.dsh}/sessions"),
            "/somewhere/else/sessions"
        );
        // Unset falls back, and the fallback's own `~` still expands.
        assert_eq!(
            expand_path("${TP_TEST_UNSET:-~/.dsh}/sessions"),
            format!("{home}/.dsh/sessions")
        );
        // A plain path is untouched apart from `~`.
        assert_eq!(
            expand_path("~/.claude/projects"),
            format!("{home}/.claude/projects")
        );
        assert_eq!(expand_path("/abs/path"), "/abs/path");
        // Unset with no fallback collapses, rather than leaving a literal
        // `${…}` that would look like a missing directory.
        assert_eq!(expand_path("${TP_TEST_UNSET}/x"), "/x");
        // An unclosed brace is text, not a panic.
        assert_eq!(expand_path("/a/${oops"), "/a/${oops");
        // Only a leading `~` is a home reference. A tilde inside a path is an
        // ordinary character in an ordinary directory name.
        assert_eq!(expand_path("/srv/data/~cache/x"), "/srv/data/~cache/x");
        assert_eq!(expand_path("~"), home.clone());
        assert_eq!(expand_path("~root/x"), "~root/x");

        // SAFETY: as above.
        unsafe { std::env::remove_var("TP_TEST_ROOT") }
    }
}

/// Load every `*.toml` in `dir`. A malformed file is skipped with a warning,
/// never fatal: one bad user-supplied config must not stop the runtimes that
/// are fine from being read.
pub fn load_configs(dir: &Path) -> Vec<DeclConfig> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str::<DeclConfig>(&s).map_err(|e| e.to_string()))
        {
            Ok(cfg) => out.push(cfg),
            Err(e) => tp_core::log_warn!("skipping runtime config {}: {e}", path.display()),
        }
    }
    out
}

pub fn default_root_for(id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    match id {
        "pi" => Path::new(&home).join(".pi").join("agent").join("sessions"),
        _ => Path::new(&home).join(".claude").join("projects"),
    }
}

#[cfg(test)]
mod loading {
    use super::*;
    use crate::adapter::decl::DeclAdapter;
    use crate::adapter::Adapter;
    use tp_core::turn::Role;

    /// A config loaded from a user directory drives the same engine the
    /// embedded ones do; a minimal descriptor exercises the defaults.
    #[test]
    fn a_toml_config_loaded_from_a_directory_parses_records() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("claude_code.toml"),
            r#"
id = "claude_code"
native_id = "stem"
role_source = "entry_type"
user_roles = ["user"]
assistant_roles = ["assistant"]
ts_path = "timestamp"
content_path = "message.content"
usage_in_path = "message.usage.input_tokens"
usage_out_path = "message.usage.output_tokens"
text_block = "text"
thinking_block = "thinking"
tool_block = "tool_use"
tool_name_field = "name"
tool_input_field = "input"
"#,
        )
        .unwrap();

        let cfgs = load_configs(dir.path());
        assert_eq!(cfgs.len(), 1, "the config must load");
        let decl = DeclAdapter::new(cfgs.into_iter().next().unwrap());

        let line = r#"{"type":"assistant","timestamp":"2026-08-03T23:30:44.000Z","message":{"content":[{"type":"thinking","thinking":"r"},{"type":"text","text":"v"}],"usage":{"input_tokens":12,"output_tokens":34}}}"#;
        let a = decl.parse_line(line).unwrap();
        assert_eq!(
            (
                a.role,
                a.text.as_str(),
                a.thinking.as_str(),
                a.tokens_in,
                a.tokens_out
            ),
            (Role::Assistant, "v", "r", Some(12), Some(34))
        );
        assert_eq!(decl.id(), "claude_code");
    }

    /// One broken config must not take the others down with it.
    #[test]
    fn a_malformed_config_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("broken.toml"),
            "id = \"x\"\nthis is not toml [[[",
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "not a config").unwrap();
        assert!(load_configs(dir.path()).is_empty());
        assert!(load_configs(Path::new("/no/such/dir")).is_empty());
    }
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;

    /// A descriptor that says nothing about itself must get the terminal
    /// harness defaults; if these drift, shipped TOMLs silently change meaning.
    #[test]
    fn omitting_the_section_reproduces_todays_behaviour() {
        let toml = r#"
            id = "x"
            native_id = "stem"
            role_source = "entry_type"
            user_roles = ["user"]
            assistant_roles = ["assistant"]
            ts_path = "timestamp"
            content_path = "message.content"
            usage_in_path = "a"
            usage_out_path = "b"
            text_block = "text"
            thinking_block = "thinking"
            tool_block = "tool_use"
            tool_name_field = "name"
            tool_input_field = "input"
        "#;
        let cfg: DeclConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.capabilities,
            Capabilities {
                scannable: true,
                multiplexed: false,
                heartbeats: false,
                pane: true,
                process_match: None,
                control_string: None,
                // No declared hint means `fl reply` and `fl ack`, correct for
                // any runtime whose shell can write the mailbox.
                reply_hint: None,
                ack_hint: None,
                non_conversation_types: Vec::new(),
            },
            "a descriptor with no [capabilities] must mean one-session-per-process, \
             scannable, pane-injectable, cannot heartbeat — i.e. Claude Code and pi"
        );
        assert_eq!(cfg.name, None, "name falls back to id at the Harness layer");
    }

    /// A GUI/multiplexed harness has to be able to say so; every field here
    /// disables one assumption core makes.
    #[test]
    fn a_multiplexed_harness_can_declare_itself() {
        let toml = r#"
            id = "dsh"
            name = "DeepSeek Harness"
            native_id = "stem"
            role_source = "entry_type"
            user_roles = ["user"]
            assistant_roles = ["assistant"]
            ts_path = "timestamp"
            content_path = "message.content"
            usage_in_path = "a"
            usage_out_path = "b"
            text_block = "text"
            thinking_block = "thinking"
            tool_block = "tool_use"
            tool_name_field = "name"
            tool_input_field = "input"

            [capabilities]
            scannable = false
            multiplexed = true
            heartbeats = true
            pane = false
        "#;
        let cfg: DeclConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("DeepSeek Harness"));
        let c = &cfg.capabilities;
        assert!(
            !c.scannable,
            "the process scan must not prune what it cannot see"
        );
        assert!(c.multiplexed, "one host process serves many sessions");
        assert!(c.heartbeats, "it renews its own presence");
        assert!(
            !c.pane,
            "no tty — a delivery channel is required, not optional"
        );
        assert_eq!(
            c.process_match, None,
            "a self-registering harness needs no signature"
        );
    }

    /// The shipped TOMLs must carry the same capabilities as the built-ins:
    /// an installed copy is the live path, so a contradiction silently changes
    /// behaviour for the runtime it replaces.
    #[test]
    fn shipped_descriptors_declare_the_same_capabilities_as_the_builtins() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d");
        for (id, builtin) in [
            ("claude_code", DeclConfig::claude_code()),
            ("pi", DeclConfig::pi()),
        ] {
            let shipped = load_configs(&dir)
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no shipped descriptor for {id}"));
            assert_eq!(
                shipped.capabilities, builtin.capabilities,
                "[{id}] shipped descriptor's capabilities diverge from the built-in"
            );
            assert!(
                shipped.name.is_some(),
                "[{id}] shipped descriptor should name itself"
            );
        }
    }

    /// The process signatures the descriptors must carry: substring for
    /// claude (a dev build with a suffixed name must match), anchored for pi
    /// (a bare substring would hit `pip`, `gpio-tool`, ...).
    #[test]
    fn the_builtins_carry_the_signatures_recognize_runtime_hardcodes() {
        assert_eq!(
            DeclConfig::claude_code()
                .capabilities
                .process_match
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            DeclConfig::pi().capabilities.process_match.as_deref(),
            Some("=pi")
        );
    }
}

#[cfg(test)]
mod embedded_configs {
    use super::*;

    /// The guard on `DeclConfig::embedded`'s `expect`: every descriptor
    /// compiled into the binary must parse and carry the id its constructor
    /// claims, so a broken file fails here rather than at first run.
    #[test]
    fn embedded_descriptors_parse() {
        for (cfg, id) in [
            (DeclConfig::claude_code(), "claude_code"),
            (DeclConfig::pi(), "pi"),
            (DeclConfig::codex(), "codex"),
        ] {
            assert_eq!(cfg.id, id);
            assert!(cfg.root.is_some(), "{id}: embedded descriptors carry roots");
        }
    }
}
