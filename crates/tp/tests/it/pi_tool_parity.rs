//! The MCP tool surface and pi's registered tools must expose the same
//! parameters.
//!
//! This is the drift the runtime-integration contract exists to prevent, and
//! it runs in one direction: `mcp.rs` gets a new capability and
//! `integrations/pi/floonet.ts` does not. A test cannot make the two
//! implementations identical, but it can make them disagree loudly.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Parameter names declared for `tool` in the MCP `inputSchema`.
fn mcp_params(src: &str, tool: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("\"name\": \"{tool}\""))
        .unwrap_or_else(|| panic!("{tool} not found in mcp.rs"));
    // Past the `"properties": {` line itself, or the container name is read as
    // a parameter.
    let schema = src[start..]
        .find("\"properties\"")
        .map(|i| start + i + "\"properties\"".len())
        .expect("inputSchema properties");
    // Ends at the close of this tool's json! block — the next tool's `"name":`
    // is a reliable terminator, and so is end-of-list.
    let end = src[schema..]
        .find("\"name\": \"floo_")
        .map(|i| schema + i)
        .unwrap_or(src.len());
    src[schema..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let key = l.strip_prefix('"')?;
            let (name, rest) = key.split_once('"')?;
            rest.trim_start()
                .starts_with(": {")
                .then(|| name.to_string())
        })
        .collect()
}

/// Parameter names declared for `tool` in dsh's `parameters: { ... }` object.
fn dsh_params(src: &str, tool: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("name: '{tool}'"))
        .unwrap_or_else(|| panic!("{tool} not found in dsh index.ts"));
    let params = src[start..]
        .find("parameters: {")
        .map(|i| start + i + "parameters: {".len())
        .expect("parameters block");
    // `output:` closes the parameters object on every tool in this file.
    let end = src[params..]
        .find("output:")
        .map(|i| params + i)
        .expect("output block");
    src[params..end]
        .lines()
        .filter_map(|l| {
            let (name, rest) = l.trim().split_once(':')?;
            (!name.is_empty()
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !rest.trim().is_empty())
            .then(|| name.to_string())
        })
        .collect()
}

/// Parameter names declared for `tool` in pi's `Type.Object({...})`.
fn pi_params(src: &str, tool: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("name: \"{tool}\""))
        .unwrap_or_else(|| panic!("{tool} not found in floonet.ts"));
    let end = src[start..]
        .find("async execute")
        .map(|i| start + i)
        .expect("execute block");
    src[start..end]
        .lines()
        .filter_map(|l| {
            let (name, rest) = l.trim().split_once(": Type.")?;
            // `parameters: Type.Object({` is the container, not a parameter.
            (!rest.starts_with("Object") && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .then(|| name.to_string())
        })
        .collect()
}

/// Deliberate, explained differences. Anything not listed here is drift.
fn allowed(tool: &str, param: &str) -> bool {
    matches!(
        (tool, param),
        // MCP is called by a model that pages by echoing back a returned cursor,
        // so it takes raw ms. pi's `until` accepts unix ms as one of its
        // spellings, so the same paging works without a second parameter.
        ("floo_turns", "before_ts")
            // MCP is a generic stdio server spawned by whichever runtime wants
            // it, so the caller has to state its own return address. pi's
            // extension runs inside pi and reads it from `ctx`.
            | ("floo_ask", "from_session")
            | ("floo_note", "from_session")
            | ("floo_reply", "from_session")
    )
}

/// Which tools each surface exposes at all, which the parameter check cannot
/// see: it compares only tools the two already share.
fn tool_names(src: &str, pat: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = src;
    while let Some(i) = rest.find(pat) {
        rest = &rest[i + pat.len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // `pat` already consumed the `floo_` prefix; stripping it again would
        // match nothing and leave both sets empty, passing vacuously.
        if !name.is_empty() {
            out.insert(name);
        }
    }
    out
}

/// Tools a surface may legitimately lack, with the reason.
fn surface_exempt(surface: &str, tool: &str) -> bool {
    matches!(
        (surface, tool),
        // Pairing and LAN discovery are administrative: they change who this
        // machine trusts, and that is a human decision made at a terminal, not
        // something an agent should reach for mid-task. Only MCP carries the
        // introduction steps, and only because Claude Code's own operator
        // drives it there. Approve and revoke are on NO surface: `fl pair`
        // subcommands only, so no agent can grant or withdraw trust.
        (
            "pi",
            "discover" | "pair_request" | "pair_list" | "pair_reject"
        ) | (
            "dsh",
            "discover" | "pair_request" | "pair_list" | "pair_reject"
        )
            // pi drains its inbox through the `/tp` skill, which shells out to
            // the CLI — the capability is present, just not as a tool. `ack`
            // is the same story: the skill calls `fl ack <id>` directly.
            | ("pi", "inbox" | "ack")
    )
}

#[test]
fn every_surface_exposes_the_same_tools() {
    let mcp = tool_names(&repo("src/mcp.rs"), "\"name\": \"floo_");
    let pi = tool_names(&repo("../../integrations/pi/floonet.ts"), "name: \"floo_");
    let dsh = tool_names(&repo("../../integrations/dsh/index.ts"), "name: 'floo_");

    let mut problems = Vec::new();
    for (surface, have) in [("pi", &pi), ("dsh", &dsh)] {
        for missing in mcp.difference(have) {
            if !surface_exempt(surface, missing) {
                problems.push(format!(
                    "  {surface} is missing floo_{missing} — a capability MCP callers have and {surface} callers do not"
                ));
            }
        }
        for extra in have.difference(&mcp) {
            problems.push(format!(
                "  MCP is missing floo_{extra}, which {surface} has — the surface a model reaches for most is the one behind"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "runtime tool surfaces have drifted:\n{}\n\nAdd the tool, or add a documented exception to `surface_exempt`.",
        problems.join("\n")
    );
}

#[test]
fn pi_tools_expose_the_same_parameters_as_the_mcp_tools() {
    let mcp = repo("src/mcp.rs");
    let pi = repo("../../integrations/pi/floonet.ts");

    let mut problems = Vec::new();
    // The messaging tools are in the loop with the retrieval ones: a mismatch
    // there loses a conversation.
    for tool in [
        "floo_search",
        "floo_sessions",
        "floo_turns",
        "floo_ask",
        "floo_note",
        "floo_reply",
    ] {
        let m = mcp_params(&mcp, tool);
        let p = pi_params(&pi, tool);
        assert!(!m.is_empty(), "parsed no MCP params for {tool}");
        assert!(!p.is_empty(), "parsed no pi params for {tool}");

        for missing in m.difference(&p) {
            if !allowed(tool, missing) {
                problems.push(format!(
                    "{tool}: pi is missing {missing:?} — a capability MCP callers have and pi callers do not"
                ));
            }
        }
        for extra in p.difference(&m) {
            if !allowed(tool, extra) {
                problems.push(format!(
                    "{tool}: pi has {extra:?} but MCP does not — same name, two surfaces"
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "MCP and pi tool surfaces have drifted:\n  {}\n\nFix integrations/pi/floonet.ts (or crates/tp/src/mcp.rs), \
         or add a documented exception to `allowed`.",
        problems.join("\n  ")
    );
}

/// One skill document, and it stays valid Agent Skills.
///
/// Two copies of the same advice drift independently, and `~/.agents/skills`
/// is scanned by pi, codex and dsh, so one file already reaches all three; a
/// per-harness copy is the obvious next move and the wrong one. The
/// frontmatter assertions are the normative constraints from
/// <https://agentskills.io/specification>, checked here rather than by
/// `skills-ref validate` so the gate holds with no network and no npm.
#[test]
fn there_is_exactly_one_skill_and_it_matches_the_agent_skills_spec() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                // `target` is build output and `.git` is history; neither ships.
                if name != "target" && name != ".git" && name != "node_modules" {
                    stack.push(p);
                }
            } else if name == "SKILL.md" {
                found.push(p);
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "expected exactly one SKILL.md — a second copy is how the last two drifted. Found: {found:#?}"
    );

    let path = &found[0];
    let text = std::fs::read_to_string(path).unwrap();
    let fm = text
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
        .map(|(fm, _)| fm)
        .expect("SKILL.md must open with YAML frontmatter");

    let field = |k: &str| {
        fm.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}: ")))
            .map(str::trim)
    };

    // name: 1-64 chars, lowercase alphanumeric and hyphens, no leading,
    // trailing or consecutive hyphens, and equal to the parent directory.
    let name = field("name").expect("`name` is required");
    assert!((1..=64).contains(&name.len()), "name length: {name:?}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
            && !name.contains("--"),
        "name must be kebab-case: {name:?}"
    );
    let parent = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert_eq!(name, parent, "spec: `name` must match the parent directory");

    // description: 1-1024 characters, non-empty. It is the ONLY thing loaded at
    // startup, so an over-long one is a real failure rather than untidiness.
    let desc = field("description").expect("`description` is required");
    assert!(
        (1..=1024).contains(&desc.len()),
        "description is {} chars, spec allows 1-1024",
        desc.len()
    );

    // The body loads whole on activation; the spec recommends under 500 lines.
    let body_lines = text.lines().count();
    assert!(
        body_lines <= 500,
        "SKILL.md is {body_lines} lines, spec recommends <=500"
    );
}

/// `allowed-tools` must cover every tool the skill's body tells the model to
/// call; split, the model asks for permission mid-task instead of doing the
/// thing. Both prefix families are kept: `mcp__floonet__*` is what a
/// directly-registered MCP server produces and `mcp__plugin_floonet_floonet__*`
/// what the plugin produces, so naming only one half-grants depending on
/// how floonet was installed.
#[test]
fn every_tool_the_skill_names_is_also_granted() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugin/skills/floonet/SKILL.md");
    let text = std::fs::read_to_string(&path).unwrap();
    let (fm, body) = text
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
        .expect("SKILL.md must open with frontmatter");

    let granted: std::collections::HashSet<&str> = fm
        .lines()
        .find(|l| l.starts_with("allowed-tools:"))
        .expect("`allowed-tools` is missing entirely")
        .split_whitespace()
        .filter(|t| t.starts_with("mcp__"))
        .filter_map(|t| t.rsplit("__").next())
        .collect();

    // The body writes the shared prefix once and then abbreviates —
    // "`floo_search`, `_sessions`, `_turns`" — so both spellings count.
    let mut named: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tok in body.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if let Some(rest) = tok.strip_prefix("floo_") {
            if !rest.is_empty() {
                named.insert(format!("floo_{rest}"));
            }
        }
    }
    for line in body.lines() {
        for tok in line.split('`') {
            if let Some(rest) = tok.strip_prefix('_') {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    named.insert(format!("floo_{rest}"));
                }
            }
        }
    }

    let ungranted: Vec<&String> = named
        .iter()
        .filter(|t| !granted.contains(t.as_str()))
        .collect();
    assert!(
        ungranted.is_empty(),
        "the skill body tells the model to call these, and allowed-tools does not grant them: {ungranted:?}"
    );
}

/// The three integrations must expose the same set of tools, not merely the
/// same parameters on the ones they happen to share. `floo_ack` is the gap
/// that matters: reading marks a message read, acking says "I finished acting
/// on it", and `inbox --pending` recovery is built on that distinction.
///
/// Two deliberate exceptions, both about who is allowed to decide what:
///
///   * `floo_pair_approve` / `floo_pair_revoke` exist nowhere and must not.
///     Granting trust has to be done by a person at a keyboard: a session
///     running with permissions skipped could otherwise make a remote machine
///     permanently able to read this one.
///   * pi's inbox is a command (`/fl`), not a tool, because that is what the
///     wake mechanism types into its pane. The capability is present; only the
///     surface differs.
#[test]
fn every_integration_exposes_the_same_tools() {
    let mcp = repo("../../crates/tp/src/mcp.rs");
    let pi = repo("../../integrations/pi/floonet.ts");
    let dsh = repo("../../integrations/dsh/index.ts");

    let names = |src: &str, pat: &str| -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = src;
        while let Some(i) = rest.find(pat) {
            rest = &rest[i + pat.len()..];
            // The pattern already consumed the `floo_` prefix, so what follows
            // is the suffix; checking for the prefix again would empty both sets.
            let suffix: String = rest
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '_')
                .collect();
            if !suffix.is_empty() {
                out.insert(format!("floo_{suffix}"));
            }
        }
        out
    };

    let mcp_tools = names(&mcp, "\"floo_");
    let pi_tools = names(&pi, "name: \"floo_");
    let mut dsh_tools = names(&dsh, "name: 'floo_");

    // pi's inbox is a command, not a tool — see the doc comment. Counted as
    // present so this test measures CAPABILITY, not registration style.
    let mut pi_tools = pi_tools;
    assert!(
        pi.contains("registerCommand(\"fl\""),
        "pi's inbox is supposed to be a command; if that went away, its inbox \
         really is missing and this allowance is hiding it"
    );
    pi_tools.insert("floo_inbox".to_string());
    dsh_tools.insert("floo_inbox".to_string()); // dsh registers it as a tool anyway

    for (who, have) in [("pi", &pi_tools), ("dsh", &dsh_tools)] {
        let missing: Vec<&String> = mcp_tools.difference(have).collect();
        assert!(
            missing.is_empty(),
            "{who} is missing tools MCP exposes: {missing:?} — not a limitation \
             of that harness, both accept registrations of exactly this shape"
        );
        let extra: Vec<&String> = have.difference(&mcp_tools).collect();
        assert!(
            extra.is_empty(),
            "{who} exposes tools MCP does not: {extra:?} — drift in the other \
             direction is still drift"
        );
    }

    // The two that must never appear anywhere.
    for forbidden in ["floo_pair_approve", "floo_pair_revoke"] {
        for (who, src) in [("mcp.rs", &mcp), ("pi", &pi), ("dsh", &dsh)] {
            assert!(
                !src.contains(forbidden),
                "{who} exposes {forbidden}: approving or revoking trust must \
                 stay a human action at a keyboard, on every surface"
            );
        }
    }
}

/// A tool description may only promise parameters the schema actually
/// declares. `floo_peers` on every surface points the model at `floo_search`'s
/// cross-machine parameters, so `floo_search` has to carry them: a model that
/// reads the promise and sends `peers` to a schema without it gets the
/// argument dropped and a LOCAL answer rendered as a cross-machine one.
#[test]
fn dsh_floo_search_declares_the_cross_machine_params_its_docs_promise() {
    let mcp = repo("src/mcp.rs");
    let dsh = repo("../../integrations/dsh/index.ts");

    let m = mcp_params(&mcp, "floo_search");
    let d = dsh_params(&dsh, "floo_search");
    assert!(!m.is_empty(), "parsed no MCP params for floo_search");
    assert!(!d.is_empty(), "parsed no dsh params for floo_search");

    let missing: Vec<&String> = m.difference(&d).collect();
    assert!(
        missing.is_empty(),
        "floo_search: dsh is missing {missing:?} — a capability MCP callers have and dsh callers do not"
    );

    // The promise itself, read out of dsh's own `floo_peers` text rather than
    // restated here, so the two cannot drift apart in opposite directions.
    let peers_desc = {
        let start = dsh.find("name: 'floo_peers'").expect("floo_peers in dsh");
        let end = start + dsh[start..].find("parameters:").expect("params");
        dsh[start..end].to_string()
    };
    for promised in ["peers", "all"] {
        if peers_desc.contains(&format!("`{promised}`")) {
            assert!(
                d.contains(promised),
                "dsh floo_peers promises `{promised}` on floo_search, whose schema does not declare it"
            );
        }
    }
}

/// The MCP `floo_inbox` schema must not name a default this surface does not
/// have. Resolution here is registry-first through the three-state
/// `own_session()`; the env var is deliberately not read, because a long-lived
/// stdio server's environment is a spawn-time snapshot that a `--resume`
/// invalidates.
#[test]
fn mcp_inbox_does_not_document_a_default_it_never_reads() {
    let mcp = repo("src/mcp.rs");
    let start = mcp
        .find("\"name\": \"floo_inbox\"")
        .expect("floo_inbox in mcp.rs");
    let end = start
        + mcp[start..]
            .find("\"name\": \"floo_ack\"")
            .expect("floo_ack");
    let schema = &mcp[start..end];

    let reads_env = mcp.contains("var(\"CLAUDE_CODE_SESSION_ID\")");
    assert!(
        reads_env || !schema.contains("CLAUDE_CODE_SESSION_ID"),
        "floo_inbox's schema promises a $CLAUDE_CODE_SESSION_ID default, and this \
         surface never reads that variable — a model told the default exists will \
         omit session_id and get an error it was told could not happen"
    );
}

/// Every wake framing must send the model through `--pending` before it drains.
///
/// An unacked message stays recoverable forever, but only via that view: a
/// batch interrupted mid-drain is invisible to a plain `fl inbox`, so a
/// framing that omits the step never looks, and the work is silently lost
/// rather than resumed. `plugin/commands/inbox.md` is canonical and has it.
#[test]
fn every_wake_framing_recovers_pending_before_draining() {
    let canonical = repo("../../plugin/commands/inbox.md");
    assert!(
        canonical.contains("--pending"),
        "the canonical wake document lost its --pending step; fix that before the copies"
    );

    // Scoped to the framing text each runtime actually puts in front of its
    // model. Searching the whole file would pass on the word "pending" in an
    // unrelated tool description and assert nothing.
    let dsh_src = repo("../../integrations/dsh/index.ts");
    let dsh_start = dsh_src.find("const FRAMING = [").expect("dsh FRAMING");
    let dsh_end = dsh_start
        + dsh_src[dsh_start..]
            .find("].join(")
            .expect("end of FRAMING");
    let dsh_framing = &dsh_src[dsh_start..dsh_end];

    let pi_src = repo("../../integrations/pi/floonet.ts");
    let pi_start = pi_src.find("pi.sendUserMessage(").expect("pi wake message");
    let pi_end = pi_start
        + pi_src[pi_start..]
            .find("deliverAs")
            .expect("end of wake message");
    let pi_framing = &pi_src[pi_start..pi_end];

    // The recovery itself runs in the handler, not as advice: neither runtime
    // can rely on the model electing to look, and pi has no `floo_inbox` tool
    // to look WITH, so a framing that merely named the view would promise a
    // surface pi does not have.
    let dsh_wake = {
        let s = dsh_src
            .find("path: '/floonet/wake'")
            .expect("dsh wake route");
        let e = s + dsh_src[s..].find("web.on('dispose'").expect("end of route");
        &dsh_src[s..e]
    };
    let pi_wake = {
        let s = pi_src
            .find("pi.registerCommand(\"fl\"")
            .expect("pi wake command");
        let e = s + pi_src[s..].find("deliverAs").expect("end of handler");
        &pi_src[s..e]
    };

    for (who, handler) in [("dsh", dsh_wake), ("pi", pi_wake)] {
        assert!(
            handler.contains("--pending"),
            "{who}'s wake handler drains without checking --pending first — a batch a \
             previous wake showed but never acked is invisible to a plain drain, and \
             this runtime would never look at it again"
        );
    }

    // And the recovered set must be labelled as older work, or the model
    // reads it as a second copy of the new messages.
    for (who, framing) in [("dsh", dsh_framing), ("pi", pi_framing)] {
        assert!(
            framing.contains("earlier wake"),
            "{who}'s framing does not tell the model that the recovered messages are \
             older, unfinished work rather than new requests"
        );
    }
}
