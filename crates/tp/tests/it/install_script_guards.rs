//! The rename migration lives in one file, and it is not the published one.
//!
//! The product was renamed teleport→floonet before release, so the only
//! machines needing migration are pre-release installs. Carrying that inline
//! in install.sh makes every reader parse removal code for a product they have
//! never seen; it lives in install/migrate-from-teleport.sh, excluded from
//! publish, and this pins the split from both sides. The needles are artifact
//! names, not the word "teleport": the data directory ~/.teleport, the launchd
//! label and the TP_* variables survive the rename on purpose.

use std::path::PathBuf;

/// Every artifact a pre-rename install left behind. One list, used from both
/// sides, so a needle cannot be dropped from one assertion and kept in the
/// other.
const PRE_RENAME_ARTIFACTS: &[&str] = &[
    "teleport@teleport", // the old Claude Code plugin id
    "TeleportPanel",     // the old menu-bar app and its login item
    // The two skill roots are separate needles: as one substring, either
    // occurrence would satisfy contains().
    ".agents/skills/teleport",   // the old shared skill
    ".pi/agent/skills/teleport", // the old pi-local skill
    "teleport.ts",               // the old pi extension
    "commands/tp.md",            // the hand-copied /tp command
    "setup-hooks",               // the pre-plugin hook writer/remover
    "\"$BIN_DIR/tp\"",           // the old binaries and their .prev generation
];

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_path(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// A file that exists in the private repository and not in the published tree.
///
/// The published tree is a projection assembled from `publish/manifest.yml`,
/// and `publish.sh` runs this suite inside it. Tests that pin the migration
/// script and the manifest return early on absence: there, absence is the
/// state they guarantee. The early return cannot pass vacuously in the private
/// tree, because the manifest marks that tree and where it exists the last
/// test asserts the script does too.
fn read_private(rel: &str) -> Option<String> {
    std::fs::read_to_string(repo_path(rel)).ok()
}

#[test]
fn the_published_installer_knows_nothing_of_the_old_name() {
    let sh = read("install/install.sh");
    for needle in PRE_RENAME_ARTIFACTS {
        assert!(
            !sh.contains(needle),
            "install.sh mentions the pre-rename artifact {needle:?} — migration \
             belongs in install/migrate-from-teleport.sh, which is not published"
        );
    }
}

#[test]
fn the_migration_script_removes_every_pre_rename_artifact() {
    let Some(sh) = read_private("install/migrate-from-teleport.sh") else {
        return; // the published tree — see `read_private`
    };
    for needle in PRE_RENAME_ARTIFACTS {
        assert!(
            sh.contains(needle),
            "migrate-from-teleport.sh no longer handles {needle:?} — if that \
             artifact class is truly gone, delete it from PRE_RENAME_ARTIFACTS \
             too; a needle in one file only is how a half-migration starts"
        );
    }
}

#[test]
fn the_migration_script_stays_out_of_the_publish_allowlist() {
    let Some(manifest) = read_private("publish/manifest.yml") else {
        return; // the published tree — see `read_private`
    };
    // Where the manifest exists this is the private repository, and the script
    // it excludes must exist to be excluded — otherwise the test above returned
    // early over a deletion rather than over a projection.
    assert!(
        repo_path("install/migrate-from-teleport.sh").exists(),
        "publish/manifest.yml is present but install/migrate-from-teleport.sh is not: \
         the migration script was deleted, and its guard test silently passed"
    );
    let excluded_at = manifest
        .find("\nexcluded:")
        .expect("manifest has an excluded section");
    let allow = &manifest[..excluded_at];
    for private in ["migrate-from-teleport.sh", "setup-hooks.py"] {
        assert!(
            !allow.contains(private),
            "{private} appears in the publish allowlist — the whole point of \
             the split is that migration never ships"
        );
        assert!(
            manifest[excluded_at..].contains(private),
            "{private} is not documented in `excluded:` — publish.sh asserts \
             excluded paths are absent from the assembled tree, so listing it \
             is what turns the intent into a check"
        );
    }
}

/// codex refuses a bare plugin name — "plugin requires --marketplace unless
/// passed as <plugin>@<marketplace>" — for both halves of its lifecycle.
/// `>/dev/null 2>&1 || true` on the uninstall half is correct tolerance and
/// also swallows a command that can never succeed, so both are pinned as a pair.
#[test]
fn every_codex_plugin_command_names_its_marketplace() {
    let script = repo_path("install/install.sh");
    let text = std::fs::read_to_string(&script)
        .unwrap_or_else(|e| panic!("read {}: {e}", script.display()));

    let mut seen = 0;
    for (n, line) in text.lines().enumerate() {
        // `plugin marketplace add|remove` takes the marketplace itself, not a
        // `<plugin>@<marketplace>` pair — matching it here would demand an
        // `@` the command rejects.
        if !line.contains("plugin add") && !line.contains("plugin remove") {
            continue;
        }
        if !line.contains("CODEX_BIN") {
            continue;
        }
        seen += 1;
        assert!(
            line.contains("floonet@floonet"),
            "install.sh:{} passes a bare plugin name to codex, which refuses it \
             — and `|| true` on this line means the failure is invisible:\n  {}",
            n + 1,
            line.trim()
        );
    }
    assert_eq!(
        seen, 2,
        "expected exactly two codex plugin commands (add on install, remove on \
         uninstall); found {seen}. A missing one is how uninstall left codex \
         holding the plugin the first time."
    );
}

/// Installing is not compiling, and the split has one dangerous half.
///
/// Default-to-not-building makes an install cheap; the staleness check is what
/// makes that default safe rather than a way to run a stale binary. Three
/// properties: `--build` exists and forces a build; staleness builds rather
/// than warns, because a warning asks a user to reason about mtimes when they
/// are not thinking about mtimes; a toolchain is required only when something
/// will be compiled.
#[test]
fn installing_does_not_mean_compiling() {
    let script = repo_path("install/install.sh");
    let text = std::fs::read_to_string(&script)
        .unwrap_or_else(|e| panic!("read {}: {e}", script.display()));

    assert!(
        text.contains("--build"),
        "there must be a way to force a build; without it the only route back \
         to a known-good binary is deleting target/"
    );
    assert!(
        text.contains("FORCE_BUILD=1"),
        "`--build` must actually set the flag, not just appear in the usage text"
    );

    // `-newer` against the installed artifact is the staleness check; a script
    // that skips the build unconditionally passes every other assertion here.
    assert!(
        text.contains("-newer \"$SRC_BIN/fl\""),
        "nothing compares the sources against the existing build — the default \
         would then install whatever happens to be in target/release"
    );

    // `--build` must be parsed before preflight consults it.
    let flag_at = text.find("FORCE_BUILD=1").expect("checked above");
    let preflight_call = text
        .rfind("\npreflight\n")
        .expect("install.sh calls preflight");
    assert!(
        flag_at < preflight_call,
        "`--build` is parsed AFTER preflight runs, so preflight cannot see it"
    );
}

/// The MCP command must be something a harness that expands nothing can spawn.
///
/// Two harnesses read these manifests and they do not agree about `${...}`.
/// codex spawns the command verbatim: `${HOME}/.local/bin/fl` becomes a literal
/// path that does not exist, the server never starts, and the only trace is a
/// line inside the harness's own log — `MCP server startup failed ... No such
/// file or directory`. Nothing in an install run says a word about it, which is
/// how it survived four days and why this test exists rather than a comment.
///
/// A bare name is correct for both: each harness spawns its servers with the
/// environment it was started from, and `install.sh` puts the binary in
/// `~/.local/bin` and offers to add that to the shell profile.
#[test]
fn the_mcp_command_survives_a_harness_that_expands_nothing() {
    let shared: serde_json::Value =
        serde_json::from_str(&read("plugin/.mcp.json")).expect("plugin/.mcp.json is JSON");
    let manifest: serde_json::Value =
        serde_json::from_str(&read("plugin/.claude-plugin/plugin.json"))
            .expect("plugin/.claude-plugin/plugin.json is JSON");

    // Two declarations of one server, in two files, read by two harnesses. They
    // drifted once already — one was fixed for a PATH problem and the other was
    // not, and the one that was "fixed" is the one its harness could not read.
    let from_shared = &shared["floonet"];
    let from_manifest = &manifest["mcpServers"]["floonet"];
    assert_eq!(
        from_shared, from_manifest,
        "the two manifests declare the same server differently; whichever a \
         harness happens to read decides whether floonet works there"
    );

    for (what, decl) in [
        ("plugin/.mcp.json", from_shared),
        ("plugin/.claude-plugin/plugin.json", from_manifest),
    ] {
        let command = decl["command"]
            .as_str()
            .unwrap_or_else(|| panic!("{what} declares no command for the floonet MCP server"));
        assert!(
            !command.contains('$'),
            "{what} interpolates into a command that is spawned verbatim by at \
             least one harness: {command:?}"
        );
        assert!(
            !command.contains('/'),
            "{what} hardcodes a path ({command:?}); the binary's location is \
             `install.sh`'s to choose, and a path here goes stale the moment it \
             does"
        );
    }
}

/// The installer asks, and the four properties that make asking safe.
///
/// Installing into a harness a person does not use leaves files they never
/// asked for, but an installer that asks can hang, refuse a script, or lose a
/// detail — so the question is fenced by four rules.
#[test]
fn the_installer_asks_only_when_someone_is_there_to_answer() {
    let text = std::fs::read_to_string(repo_path("install/install.sh")).expect("install.sh");

    // 1. No terminal, no question. A pipe, a CI job or a launchd invocation
    //    behaves as if nobody answered. The test is on stdin, not stdout: output
    //    is routinely redirected to a log while the operator is still there.
    assert!(
        text.contains("[ -t 0 ] || INTERACTIVE=0"),
        "nothing forces non-interactive without a terminal — an installer that \
         blocks on a question nobody can see is worse than one that never asks"
    );
    // Code only: the script's own comment names `[ -t 1 ]` to say why it is
    // not used, and a substring search cannot tell explanation from
    // implementation.
    let code: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("[ -t 1 ]"),
        "interactivity must not be decided by stdout: `| tee install.log` is \
         normal and does not mean nobody is watching"
    );

    // 2. THE MANDATORY PARTS ARE NOT OFFERED. Only the four integrations are
    //    choices. The binaries, the LaunchAgent and the panel are floonet —
    //    offering them would be offering to install something that cannot work.
    for gated in ["wants claude", "wants codex", "wants pi", "wants dsh"] {
        assert!(
            text.contains(gated),
            "{gated} is missing: the four integrations are the selectable set"
        );
    }
    for never in ["wants panel", "wants daemon", "wants fld", "wants binaries"] {
        assert!(
            !text.contains(never),
            "{never} appeared — the daemon and panel are not accessories. \
             Without the daemon nothing is discovered; without the panel a \
             person cannot see what is live."
        );
    }

    // 3. Asked before anything is touched, anchored on the first side effect
    //    rather than on `preflight`: preflight only checks and exits, so being
    //    after it costs nothing, while being after the binary install costs
    //    everything.
    let asked = text
        .find("Which integrations")
        .expect("the selection block");
    let first_side_effect = text
        .find("mkdir -p \"$BIN_DIR\"")
        .expect("install.sh creates BIN_DIR before writing anything");
    assert!(
        asked < first_side_effect,
        "the selection happens after the first side effect, so a rejected \
         `--only` still replaces binaries and restarts the daemon first"
    );

    // 4. AN UNAVAILABLE NAME IS AN ERROR. `--only codex` where codex is absent
    //    means the caller believes something false; finishing quietly would let
    //    them keep believing it.
    assert!(
        text.contains("which is not available here"),
        "`--only` silently ignores names it cannot satisfy"
    );
}
