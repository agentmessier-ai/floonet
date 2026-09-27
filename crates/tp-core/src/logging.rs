//! The logging contract, for something that ships to a laptop.
//!
//! The fleet contract assumes a collector: JSON by default, no timestamp on
//! the line. Floonet has no collector — its log is read by a person, usually
//! while something is wrong — so the default is human-readable with a
//! timestamp and `TP_LOG_FORMAT=json` opts in. Both formats go to stderr:
//! `tp`'s stdout is its product, and launchd already routes `fld`'s stderr to
//! a file. No trace/span fields: there is no tracer on a laptop. Redaction
//! reaches named fields only; a secret inside the message text is opaque by
//! the time it arrives, so pass it as a field.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static SERVICE: OnceLock<String> = OnceLock::new();

/// Where lines go when a binary has claimed a file. `None` = stderr.
static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Rotate at 5 MB and keep one previous file, so a daemon costs at most 10 MB
/// on someone else's disk. Every loop that can log in a failure has a sleep
/// floor, so this is years of healthy operation and weeks of a broken one:
/// large enough that rotation does not destroy evidence, small enough to
/// bound the disk.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

struct Sink {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
}

/// Name this process in its own log lines. Called once at startup by a binary.
///
/// Set in code rather than in the LaunchAgent plist because the plist applies
/// only when launchd starts it; `fld` run from a terminal would otherwise be
/// indistinguishable from the CLI. `TP_SERVICE_NAME` still wins, so an
/// operator can override without a rebuild.
pub fn set_service(name: &str) {
    let _ = SERVICE.set(name.to_string());
}

/// Keys whose values must never reach a log line. Matched case-insensitively.
pub const REDACT_KEYS: &[&str] = &[
    "authorization",
    "cookie",
    "token",
    "api_key",
    "apikey",
    "password",
    "secret",
    "bearer_token",
    "access_token",
    "refresh_token",
    "set-cookie",
];

const CENSOR: &str = "[redacted]";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Lowercase in JSON, because the contract's claim is that several services
    /// are one queryable dataset and a query written level="error" must match.
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Padded and upper for the human format, so the message column lines up
    /// down the page — the reason to read this file at all is to scan it.
    fn as_column(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }

    fn from_env() -> Level {
        match env_prefixed("TP_LOG_LEVEL", "LOG_LEVEL")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "debug" | "trace" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
}

fn is_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    REDACT_KEYS.iter().any(|k| lower == *k)
        || lower.ends_with("_token")
        || lower.ends_with("_secret")
        // Suffix rules catch `private_key`, `signing_key`, `github_token` and
        // the like without a list entry per leak class.
        || lower.ends_with("_key")
}

/// The `TP_`-prefixed name wins; the bare legacy name is read only as a
/// fallback for pre-namespace configurations, not as a second contract.
/// `LOG_LEVEL` in particular is generic enough that an unrelated ancestor
/// exporting it must not be able to steer this logger.
fn env_prefixed(prefixed: &str, legacy: &str) -> Option<String> {
    match std::env::var(prefixed) {
        Ok(v) => Some(v),
        Err(_) => std::env::var(legacy).ok(),
    }
}

fn service() -> String {
    match env_prefixed("TP_SERVICE_NAME", "SERVICE_NAME") {
        Some(v) => v,
        None => SERVICE
            .get()
            .cloned()
            .unwrap_or_else(|| "floonet".to_string()),
    }
}

/// Write to `path` instead of stderr, rotating it. Called once at startup by a
/// daemon; a CLI must not, because its lines belong on the terminal.
///
/// The daemon owns this file rather than truncating the one launchd opened for
/// its stderr: floonet does not control that descriptor's flags, and without
/// `O_APPEND` a truncation leaves the next write at the old offset behind a
/// hole. launchd's file stays as the capture for what this logger cannot see —
/// a panic, a dyld failure, anything before `init`.
pub fn to_file(path: &Path) -> std::io::Result<()> {
    // Two owners for one log line is a caller bug; an `Ok` here would let the
    // caller believe lines go to the path it named when they go to the first sink.
    if SINK.get().is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "log sink already configured",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    SINK.set(Mutex::new(Sink {
        path: path.to_path_buf(),
        file,
        written,
    }))
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "log sink claimed concurrently",
        )
    })?;
    Ok(())
}

/// Move the current file aside and start a new one. One generation: `.1` is
/// overwritten, because two stale copies answer no question the one does not.
fn rotate(sink: &mut Sink) -> std::io::Result<()> {
    let previous = sink.path.with_extension("log.1");
    std::fs::rename(&sink.path, &previous)?;
    sink.file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sink.path)?;
    sink.written = 0;
    Ok(())
}

/// Bound a log file this process does not own — launchd's `StandardErrorPath`.
///
/// Nothing writes to that file in normal operation; the case that does is a
/// daemon that cannot start, which `KeepAlive` respawns forever, each attempt
/// appending its error before `to_file` is reached. Renamed, never truncated:
/// launchd holds the descriptor, and it follows the inode, so this run's stderr
/// lands in `.1` until the next spawn — cheaper than unbounded growth. Call at
/// startup before `to_file`; failure is not reported, because a daemon must
/// not decline to start over housekeeping.
pub fn bound_foreign_log(path: &Path, max: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < max {
        return;
    }
    let _ = std::fs::rename(path, path.with_extension("log.1"));
}

/// `TP_LOG_FORMAT=json` for the machine-readable form (bare `LOG_FORMAT`
/// still read as fallback). Anything else, including unset, gives the human
/// one — the inverse of the fleet default, because the reader here is a
/// person and not a collector.
fn want_json() -> bool {
    env_prefixed("TP_LOG_FORMAT", "LOG_FORMAT")
        .map(|f| f.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

/// One line. Called by the macros below; call it directly when you have fields
/// worth naming, which is the only form redaction can reach.
pub fn emit(level: Level, msg: &str, fields: &[(&str, &str)]) {
    if level < Level::from_env() {
        return;
    }
    let redacted = |k: &str, v: &str| -> String {
        if is_secret(k) {
            CENSOR.to_string()
        } else {
            v.to_string()
        }
    };
    // Local time, not UTC: this is read by the person sitting at the machine
    // that wrote it, next to timestamps from their own shell.
    let now = chrono::Local::now();

    let line = if want_json() {
        let mut map = serde_json::Map::new();
        // `ts` first, and present at all — the fleet logger omits it because a
        // collector stamps at ingest. Nothing stamps this one.
        map.insert("ts".into(), now.to_rfc3339().into());
        map.insert("service".into(), service().into());
        map.insert("level".into(), level.as_str().into());
        map.insert("msg".into(), msg.into());
        for (k, v) in fields {
            map.insert((*k).to_string(), redacted(k, v).into());
        }
        serde_json::Value::Object(map).to_string()
    } else {
        // Continuation lines are indented rather than escaped: a multi-line
        // message (a TOML parse error with its caret diagram) stays readable
        // and stays one event for a `grep` on the level.
        let mut out = format!(
            "{} {} {}",
            now.format("%Y-%m-%d %H:%M:%S"),
            level.as_column(),
            msg.trim_end().replace('\n', "\n    ")
        );
        // Fields trail the message as `k=v`, so the message a person scans
        // for starts at a fixed column.
        for (k, v) in fields {
            out.push_str(&format!(" {k}={}", redacted(k, v)));
        }
        out
    };

    // A log write must never fail the thing it logs, so every failure below
    // is swallowed, including a rotation that cannot rename.
    match SINK.get() {
        Some(sink) => {
            if let Ok(mut sink) = sink.lock() {
                // Rotate before writing, so the cap is a cap rather than a
                // threshold the last line is allowed to cross.
                if sink.written >= MAX_BYTES {
                    let _ = rotate(&mut sink);
                }
                if writeln!(sink.file, "{line}").is_ok() {
                    sink.written += line.len() as u64 + 1;
                }
            }
        }
        None => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{line}");
        }
    }
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Warn, &format!($($arg)*), &[])
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Error, &format!($($arg)*), &[])
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Info, &format!($($arg)*), &[])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redaction is by field name, in both formats.
    #[test]
    fn secrets_are_censored_by_name_not_by_value() {
        for (name, secret) in [
            ("token", true),
            ("Authorization", true),
            ("github_token", true),
            ("client_secret", true),
            ("private_key", true),
            ("signing_key", true),
            ("path", false),
            ("session_id", false),
        ] {
            assert_eq!(is_secret(name), secret, "{name}");
        }
    }

    /// `INFO ` and `WARN ` are padded so the message starts at one column. A
    /// log a person scans is the whole reason for the human format.
    #[test]
    fn the_level_column_is_fixed_width() {
        let widths: Vec<usize> = [Level::Debug, Level::Info, Level::Warn, Level::Error]
            .iter()
            .map(|l| l.as_column().len())
            .collect();
        assert_eq!(widths, vec![5, 5, 5, 5]);
    }

    /// Over the cap the file moves aside; under it, it is left alone — a
    /// small one holds the crash output someone is trying to read.
    #[test]
    fn the_launchd_error_file_is_bounded_but_a_small_one_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("tp-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Under the cap: untouched, not rotated.
        let small = dir.join("small.log");
        std::fs::write(&small, b"a panic worth reading\n").unwrap();
        bound_foreign_log(&small, 1024);
        assert_eq!(
            std::fs::read_to_string(&small).unwrap(),
            "a panic worth reading\n",
            "a small file must be left exactly as it was"
        );
        assert!(
            !dir.join("small.log.1").exists(),
            "nothing to rotate — a .1 here means the cap was ignored"
        );

        // Over the cap: moved aside, contents preserved under `.1`.
        let big = dir.join("big.log");
        std::fs::write(&big, vec![b'x'; 2048]).unwrap();
        bound_foreign_log(&big, 1024);
        assert!(
            !big.exists(),
            "the oversized file must be moved, not left in place"
        );
        assert_eq!(
            std::fs::metadata(dir.join("big.log.1")).unwrap().len(),
            2048,
            "renamed, never truncated — launchd owns the descriptor"
        );

        // A file that is not there at all is not an error: the daemon may never
        // have run under launchd.
        bound_foreign_log(&dir.join("absent.log"), 1024);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Driven through `rotate` directly rather than by writing 5 MB: the cap
    /// is a constant, and the property under test is the rename.
    #[test]
    fn rotation_keeps_one_generation_and_loses_nothing() {
        let dir = std::env::temp_dir().join(format!("tp-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tpd.log");
        std::fs::write(&path, b"first generation\n").unwrap();

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut sink = Sink {
            path: path.clone(),
            file,
            written: MAX_BYTES,
        };
        rotate(&mut sink).unwrap();
        writeln!(sink.file, "second generation").unwrap();

        assert_eq!(sink.written, 0, "the byte count restarts with the file");
        assert_eq!(
            std::fs::read_to_string(dir.join("tpd.log.1")).unwrap(),
            "first generation\n",
            "the previous generation is kept, not deleted"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "second generation\n",
            "and the live file starts clean"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second `to_file` is a caller bug and must say so.
    ///
    /// Claims the process-wide `SINK` and does not release it; safe only while
    /// no other test in this binary calls `emit`.
    #[test]
    fn the_sink_accepts_one_owner() {
        let dir = std::env::temp_dir().join(format!("tp-sink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(to_file(&dir.join("once.log")).is_ok());
        let err = to_file(&dir.join("twice.log")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JSON is opt-in, the inverse of the fleet default: there is no collector
    /// on a laptop to prefer it for.
    #[test]
    fn json_is_opt_in_and_anything_else_is_human() {
        // Not a set of aliases: only the exact word, so a typo gives the
        // readable form rather than silently the parsed one.
        for (value, json) in [
            ("json", true),
            ("JSON", true),
            ("pretty", false),
            ("", false),
            ("jsonl", false),
        ] {
            std::env::set_var("TP_LOG_FORMAT", value);
            assert_eq!(want_json(), json, "TP_LOG_FORMAT={value:?}");
        }
        std::env::remove_var("TP_LOG_FORMAT");
        assert!(!want_json(), "unset must mean human");

        // The pre-namespace bare name still works — a fallback, not a second
        // contract — and the prefixed one would beat it if both were set.
        std::env::set_var("LOG_FORMAT", "json");
        assert!(want_json(), "legacy LOG_FORMAT must still be honoured");
        std::env::set_var("TP_LOG_FORMAT", "pretty");
        assert!(!want_json(), "the TP_ name must win over the legacy one");
        std::env::remove_var("TP_LOG_FORMAT");
        std::env::remove_var("LOG_FORMAT");
    }
}
