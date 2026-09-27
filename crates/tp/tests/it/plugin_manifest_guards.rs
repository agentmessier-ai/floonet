//! `plugin/hooks/hooks.json` is parsed by two harnesses, and only one is
//! forgiving. Claude Code ignores fields it does not know; codex rejects the
//! whole file on an unknown top-level field, and then no hook registers: the
//! session never calls `fl register`, never gets a `live_session` row, and an
//! `fl ask` addressed to it wakes nothing. Prose belongs in
//! `plugin/hooks/README.md`. This is a whitelist rather than a ban on
//! `_comment`: the next comment key will be spelled differently, and a test
//! that bans one spelling catches none of them.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// codex's parser names exactly two fields. Anything else is a load failure,
/// not a warning.
const ALLOWED_TOP_LEVEL: [&str; 2] = ["description", "hooks"];

#[test]
fn the_hooks_manifest_carries_only_fields_codex_accepts() {
    let path = repo_root().join("plugin/hooks/hooks.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));

    let obj = v.as_object().expect("hooks.json is a JSON object");
    let unknown: Vec<&String> = obj
        .keys()
        .filter(|k| !ALLOWED_TOP_LEVEL.contains(&k.as_str()))
        .collect();
    assert!(
        unknown.is_empty(),
        "codex rejects the WHOLE file on an unknown top-level field, so these \
         silently unregister every codex session: {unknown:?}. It accepts only \
         {ALLOWED_TOP_LEVEL:?} — put prose in plugin/hooks/README.md instead."
    );

    // The point of the file. A whitelist that passed on an empty manifest would
    // be a test that only ever fails when someone adds a comment.
    let hooks = obj
        .get("hooks")
        .expect("no `hooks` key — nothing registers");
    // UserPromptSubmit is the renewal: SessionStart fires once and there is no
    // heartbeat, so without it a session alive when the database is recreated
    // is unreachable for the rest of its life.
    for event in ["SessionStart", "SessionEnd", "UserPromptSubmit"] {
        assert!(
            hooks.get(event).is_some(),
            "`{event}` is missing: without both, a session either never becomes \
             reachable or never stops being listed as live"
        );
    }

    // The renewal, and ONLY the renewal, must be unable to interrupt a turn.
    // A payload one harness shapes differently than the other would otherwise
    // put an error banner on every prompt.
    let renew = hooks["UserPromptSubmit"].to_string();
    assert!(
        renew.contains("register --from-hook") && renew.contains("|| true"),
        "the per-prompt renewal must re-register and must never fail a turn:\n{renew}"
    );
    let start = hooks["SessionStart"].to_string();
    assert!(
        !start.contains("|| true"),
        "SessionStart must NOT be silenced — if it fails the session is \
         unreachable and someone has to know:\n{start}"
    );
    assert!(
        raw.contains("register --from-hook"),
        "the registration command is what makes a session addressable:\n{raw}"
    );
}

/// The rationale lives in the README, and must stay there: without it the next
/// person to touch these hooks has no record of why `--runtime` is deliberately
/// absent, and adding it would mislabel one harness's sessions as the other's.
#[test]
fn the_rationale_survived_the_move() {
    let readme = repo_root().join("plugin/hooks/README.md");
    let text = std::fs::read_to_string(&readme)
        .unwrap_or_else(|e| panic!("read {}: {e}", readme.display()));

    for needle in [
        // Why the file cannot carry comments.
        "_comment",
        // Why there is no --runtime flag: with one, codex sessions would be
        // composed as <machine>/claude_code/<codex id>.
        "--runtime",
        // How to tell codex actually accepted the hooks.
        "fl live",
    ] {
        assert!(
            text.contains(needle),
            "plugin/hooks/README.md no longer explains {needle:?} — that \
             reasoning is not recoverable from the JSON it was moved out of"
        );
    }
}
