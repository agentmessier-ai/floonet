//! Stamps the build with the commit it came from.
//!
//! The workspace semver does not change between rebuilds, so it cannot say
//! whether a running daemon or a peer is the code just built. The short
//! commit can. Everything in `version.rs` builds on these two variables.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves, so a rebuild after a commit does not keep
    // stamping the old sha. `.git/HEAD` covers commits and branch switches;
    // packed-refs and the ref file cover the rest.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");

    println!("cargo:rustc-env=TP_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=TP_BUILD_DATE={}", build_date());
}

/// The short commit, suffixed `-dirty` when the tree has uncommitted changes.
/// Outside a git checkout (a source tarball) this is `unknown` rather than a
/// build failure: a vague version string is the lesser harm.
fn git_sha() -> String {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(sha) = sha else {
        return "unknown".to_string();
    };

    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if dirty {
        format!("{sha}-dirty")
    } else {
        sha
    }
}

/// UTC date of the build, `YYYY-MM-DD`. A date rather than a timestamp: it
/// tells two builds of one commit apart for a human, and a full timestamp
/// would change on every rebuild and defeat build caching for no gain.
fn build_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_from_days(secs / 86_400)
}

/// Howard Hinnant's `civil_from_days`, the standard proleptic-Gregorian
/// conversion. Written out rather than pulling `chrono` into a build script
/// that would then have to compile before anything else in the workspace.
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
