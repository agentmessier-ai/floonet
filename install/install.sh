#!/bin/bash
# Install fl + fld and register the LaunchAgent.
#
# Usage:  ./install/install.sh                install and start
#         ./install/install.sh --build        compile first, even if current
#         ./install/install.sh --yes          take every default, ask nothing
#         ./install/install.sh --only a,b     install exactly these integrations
#         ./install/install.sh --uninstall   remove the program; asks about data
#         ./install/install.sh --uninstall --purge   and the data, unasked
set -euo pipefail

# Optional: the four agent integrations. Which harnesses a person uses is their
# business, and installing into one they do not run leaves files they never
# asked for.
#
# Not optional: the `fl`/`fld` binaries, the `fld` LaunchAgent, and the menu
# bar panel. Without the daemon nothing is discovered and no message is
# delivered; without the panel a person cannot see which sessions are live.
# Offering those as choices would be offering to install something that does
# not work.
#
# Asking is conditional and never required: a terminal gets a prompt; a pipe,
# a CI job or `--yes` installs whatever is detected. An installer that blocks
# on a question nobody is there to read is worse than one that never asks.
INTERACTIVE=1
ONLY=""
PURGE=0
UNINSTALL=0

# Installing is not compiling. The default installs what is in target/release
# and builds only when nothing is there or it is older than the sources;
# `--build` forces a build regardless.
#
# The staleness check builds rather than warns: the point of an install step
# is that afterwards the machine runs what the tree says, and a warning here
# asks the user to reason about mtimes.
FORCE_BUILD=0

LABEL="io.teleport.tpd"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BIN_DIR="$HOME/.local/bin"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Checked here rather than by `rust-version` in Cargo.toml: that field is only
# consulted after every manifest in the graph parses, and on an old toolchain
# the parse of a transitive dependency using a newer edition fails first, with
# an error that never names floonet and suggests nightly when stable would do.
MIN_RUST="1.88.0"

# `command -v` alone is not enough: a login shell, a script over ssh and a
# launchd job see three different PATHs, and telling someone with a working
# cargo that Rust is not installed sends them to fix a problem they do not have.
find_cargo() { find_tool cargo "$HOME/.cargo/bin/cargo"; }

# The same problem as `find_cargo`, for the tools this script only detects. A
# script run over ssh has no ~/.local/bin on PATH, and detection failing
# quietly is worse than the tool being absent: absent is the truth, and a
# skipped integration announced by one `note:` line is not.
find_tool() {
    local name="$1"; shift
    local c
    for c in "$name" "$@" "$HOME/.local/bin/$name" /usr/local/bin/"$name" \
             /opt/homebrew/bin/"$name"; do
        if command -v -- "$c" >/dev/null 2>&1; then
            command -v -- "$c"
            return 0
        fi
    done
    return 1
}

# `sort -V`, not three integer comparisons: hand-rolled component compare is
# where this check usually goes wrong (1.10 vs 1.9), and every machine that can
# run this script has sort.
version_lt() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ]
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build) FORCE_BUILD=1; shift ;;
        # `--yes` and a non-tty are the same decision spelled two ways, so they
        # set the same flag: take the defaults, say what was taken, do not wait.
        --yes|-y) INTERACTIVE=0; shift ;;
        # Pre-answers the integrations question and nothing else. It does not
        # clear INTERACTIVE: a person typing `--only pi` in a terminal is still
        # a person, and still wants to be asked before their shell profile is
        # edited.
        --only) ONLY="${2:-}"; shift 2 ;;
        # Widen --uninstall to take the binaries and ~/.teleport as well. Typed,
        # never inferred: the device key is in there.
        --purge) PURGE=1; shift ;;
        # A flag rather than a positional check, so `--uninstall --purge`
        # parses in either order.
        --uninstall) UNINSTALL=1; shift ;;
        *) break ;;
    esac
done
# No terminal, no question. `[ -t 0 ]` and not `[ -t 1 ]`: output is routinely
# piped to a log while the operator is still sitting there, and it is INPUT that
# decides whether an answer can arrive.
[ -t 0 ] || INTERACTIVE=0



SRC_BIN="$REPO/target/release"

# Is the existing build usable, and is it current?
#
# `find -newer` answers the only question that matters: does any source file
# postdate the binary. Scoped to what goes into the binary (crates/, the
# manifests, the lockfile, the toolchain pin); descriptors and the plugin tree
# are data this script copies separately, and treating them as build inputs
# would rebuild the world every time a comment moved.
needs_build() {
    [ "$FORCE_BUILD" = "1" ] && { echo "--build"; return 0; }
    [ -x "$SRC_BIN/fl" ] && [ -x "$SRC_BIN/fld" ] || { echo "nothing built yet"; return 0; }
    local newer
    # `-type f`, because a directory's mtime changes when anything is added to
    # it and "crates/tp/tests is newer" names nothing a person can look at.
    # Tests are still counted: excluding them means reasoning about which files
    # are inputs, and being wrong in that direction installs a stale binary.
    newer="$(find "$REPO/crates" "$REPO/Cargo.toml" "$REPO/Cargo.lock" \
        "$REPO/rust-toolchain.toml" -type f -newer "$SRC_BIN/fl" -print -quit 2>/dev/null || true)"
    if [ -n "$newer" ]; then
        # Name the file. "Sources changed" on a tree someone did not touch is
        # the kind of message that gets ignored until it is wrong.
        echo "newer than the build: ${newer#"$REPO"/}"
        return 0
    fi
    return 1
}

preflight() {
    # A source tree builds; an unpacked release archive already has what it
    # needs. `release.yml` ships install.sh next to `fl` and `fld` with no
    # crates, so demanding a toolchain there would be asking for a compiler to
    # build something already built.
    if [ ! -f "$REPO/Cargo.toml" ]; then
        if [ -x "$REPO/fl" ] && [ -x "$REPO/fld" ]; then
            PREBUILT=1
            return 0
        fi
        echo "install.sh cannot tell what it is installing FROM." >&2
        echo "  Expected either $REPO/Cargo.toml (a source checkout)" >&2
        echo "  or fl and fld next to this script (an unpacked release)." >&2
        echo "  Found neither. If you copied install.sh on its own, take the" >&2
        echo "  whole tree: https://github.com/agentmessier-ai/floonet" >&2
        exit 1
    fi
    PREBUILT=0

    # A toolchain is required to build, not to install. Demanding one when
    # nothing is going to be compiled turns "copy two files I already have"
    # into "install Rust first". The `--build` path still lands here, because
    # `needs_build` reports true for it.
    if ! needs_build >/dev/null; then
        return 0
    fi

    # CARGO_BIN, not a local: the build must call the same cargo preflight
    # approved. A non-interactive shell does not source the profile that puts
    # cargo on PATH, so a bare `cargo` later can fail on a toolchain preflight
    # found.
    local ver
    if ! CARGO_BIN="$(find_cargo)"; then
        # Two ways out, cheapest first: a release archive needs no toolchain,
        # because it ships `fl`, `fld` and a built panel.
        echo "This is a source checkout with nothing built, so installing from it" >&2
        echo "needs a Rust toolchain. There are two ways forward:" >&2
        echo >&2
        echo "  1. Download a release instead — no toolchain, nothing to compile:" >&2
        echo "       https://github.com/agentmessier-ai/floonet/releases" >&2
        echo "     Unpack it and run the install.sh inside." >&2
        echo >&2
        echo "  2. Build from this checkout:" >&2
        echo "       curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        echo "       $0" >&2
        echo >&2
        echo "  rustup specifically, not Homebrew's rust: this repository pins its" >&2
        echo "  toolchain in rust-toolchain.toml, which only rustup reads. With" >&2
        echo "  rustup the right version is fetched for you." >&2
        exit 1
    fi
    ver="$("$CARGO_BIN" --version 2>/dev/null | awk '{print $2}')"
    if [ -z "$ver" ]; then
        echo "warning: $CARGO_BIN did not report a version — continuing anyway." >&2
        return 0
    fi
    if version_lt "$ver" "$MIN_RUST"; then
        # Name the version AND the path. "Rust is too old" is unactionable on a
        # machine with three of them installed.
        echo "floonet needs Rust >= $MIN_RUST; this shell has $ver ($CARGO_BIN)." >&2
        if command -v rustup >/dev/null 2>&1; then
            echo "  rustup is managing this toolchain. Run:  rustup update" >&2
            echo "  rust-toolchain.toml then pins the build to the right version." >&2
        else
            echo >&2
            echo "  There is no rustup here, so rust-toolchain.toml — which pins the" >&2
            echo "  version this is tested at — does nothing on this machine." >&2
            echo "  Installing rustup fixes both at once:" >&2
            echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        fi
        exit 1
    fi
}
preflight

# `bootout` on a service that isn't loaded exits non-zero — that is a normal
# state here, not a failure, so it must not trip `set -e`.
unload() { launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true; }

if [ "$UNINSTALL" = "1" ]; then
    unload
    rm -f "$PLIST"
    # The plugin owns the hooks, /floonet:inbox, the skill and the MCP entry,
    # so removing it removes all four. Guarded on `claude` existing, and
    # tolerant of failure: a machine that never had the plugin has nothing to
    # uninstall, and that is not worth stopping a teardown for.
    if CLAUDE_BIN="$(find_tool claude)"; then
        "$CLAUDE_BIN" plugin uninstall floonet@floonet >/dev/null 2>&1 || true
        "$CLAUDE_BIN" plugin marketplace remove floonet >/dev/null 2>&1 || true
    fi
    # codex, the same way: it is installed through `codex plugin add`, so it
    # comes back out through `codex plugin remove`. `floonet@floonet`, not
    # `floonet`: codex refuses a bare name without `--marketplace`, and with
    # output suppressed and `|| true` a command that can never succeed would
    # hide inside the tolerance.
    if CODEX_BIN="$(find_tool codex)"; then
        "$CODEX_BIN" plugin remove floonet@floonet >/dev/null 2>&1 || true
        "$CODEX_BIN" plugin marketplace remove floonet >/dev/null 2>&1 || true
    fi
    # dsh, the same way: `dsh plugin` owns the profile manifest, so the plugin
    # comes back out through it rather than by deleting files underneath it.
    # Only the default profile is attempted — the name is the user's choice and
    # nothing here can enumerate it — so a plugin in a differently-named profile
    # survives, which is why this is not announced as a complete removal.
    if DSH_BIN="$(find_tool dsh)"; then
        "$DSH_BIN" plugin --profile web remove @floonet/dsh >/dev/null 2>&1 || true
    fi
    rm -f "$HOME/.pi/agent/extensions/floonet.ts"
    rm -rf "$HOME/.pi/agent/skills/floonet"   # older location
    rm -rf "$HOME/.agents/skills/floonet"
    # Only the configs THIS repo ships — a config the operator wrote for their
    # own runtime lives in the same directory and is not ours to delete.
    for f in "$REPO/install/runtimes.d/"*.toml; do
        rm -f "$HOME/.teleport/runtimes.d/$(basename "$f")"
    done
    # The panel is ours: install.sh put it in /Applications, so --uninstall
    # takes it back, the same way it hands the Claude Code plugin back to
    # `claude plugin uninstall`. Quit first: deleting a running .app leaves it
    # in the menu bar with its bundle gone.
    if [ -d "/Applications/FloonetPanel.app" ]; then
        osascript -e 'tell application "FloonetPanel" to quit' >/dev/null 2>&1 || true
        rm -rf "/Applications/FloonetPanel.app"
        # The login item outlives the bundle, and a stale one makes macOS
        # complain at every login about an item it cannot find.
        osascript -e 'tell application "System Events" to delete login item "FloonetPanel"' \
            >/dev/null 2>&1 || true
        echo "removed /Applications/FloonetPanel.app and its login item"
    fi
    # A daemon started by hand outlives `launchctl bootout`, which only knows
    # about the job it manages, and would keep the socket and the database the
    # message below says can be deleted. Reported, not killed: a process this
    # script did not start is not this script's to end.
    if pgrep -f "$BIN_DIR/fld" >/dev/null 2>&1; then
        echo "  note: fld is STILL RUNNING (pid $(pgrep -f "$BIN_DIR/fld" | tr '\n' ' '))." >&2
        echo "        It was not started by the LaunchAgent this removed, so stopping" >&2
        echo "        it is yours:  pkill -f '$BIN_DIR/fld'" >&2
    fi
    # ── What is left, and whether to keep it ─────────────────────────────────
    #
    # Two things survive removing the program: the two binaries, and
    # ~/.teleport. The cost is stated before the question, because "delete
    # your data? [y/N]" is not answerable and the counts are — the device key
    # especially: it is this machine's identity, and regenerating it
    # invalidates every pairing on both sides.
    #
    # Non-interactive removes nothing extra: `--purge` is the only way to
    # widen a scripted teardown, and it has to be typed.
    local_counts() {
        command -v sqlite3 >/dev/null 2>&1 || return 1
        [ -f "$HOME/.teleport/teleport.db" ] || return 1
        sqlite3 "$HOME/.teleport/teleport.db" \
            "SELECT (SELECT COUNT(*) FROM message) || ' messages, ' ||
                    (SELECT COUNT(*) FROM conversation) || ' conversation threads, ' ||
                    (SELECT COUNT(*) FROM machine WHERE is_self = 0) || ' paired machines';" 2>/dev/null
    }

    DROP_DATA=0
    DROP_BINS=0
    if [ "$PURGE" = "1" ]; then
        DROP_DATA=1; DROP_BINS=1
    elif [ "$INTERACTIVE" = "1" ]; then
        echo
        echo "Left behind:"
        echo "  $BIN_DIR/fl, $BIN_DIR/fld   the binaries"
        if c="$(local_counts)"; then
            echo "  ~/.teleport                  $c"
        else
            echo "  ~/.teleport                  the database and the device key"
        fi
        echo "  ~/.teleport/key              this machine's identity — every peer that"
        echo "                               trusts you trusts THIS key. A new one is a"
        echo "                               new machine, and every pairing is void."
        echo
        printf 'Remove the binaries too? [Y/n] '
        read -r a || a=""
        case "$a" in [Nn]*) ;; *) DROP_BINS=1 ;; esac
        printf 'Remove the data as well? [y/N] '
        read -r a || a=""
        case "$a" in [Yy]*) DROP_DATA=1 ;; *) ;; esac
        echo
    fi

    if [ "$DROP_BINS" = "1" ]; then
        rm -f "$BIN_DIR/fl" "$BIN_DIR/fld" "$BIN_DIR/.fl.prev" "$BIN_DIR/.fld.prev"
        echo "removed the binaries from $BIN_DIR"
    fi
    if [ "$DROP_DATA" = "1" ]; then
        rm -rf "$HOME/.teleport"
        echo "removed ~/.teleport — index, mailbox, pairings and device key"
    fi

    echo "uninstalled."
    [ "$DROP_BINS" = "0" ] && echo "  binaries kept in $BIN_DIR"
    [ "$DROP_DATA" = "0" ] && echo "  data kept in ~/.teleport   (--purge, or rm -rf ~/.teleport)"
    exit 0
fi

# ── Which integrations ───────────────────────────────────────────────────────
#
# Detect first, then ask — and only ever offer what is actually here. A menu
# listing codex on a machine with no codex is asking someone to decide something
# they cannot act on, and the answer would be wrong either way.
AVAIL=()
CLAUDE_BIN=""; CODEX_BIN=""
if CLAUDE_BIN="$(find_tool claude)"; then AVAIL+=(claude); else CLAUDE_BIN=""; fi
if CODEX_BIN="$(find_tool codex)";  then AVAIL+=(codex);  else CODEX_BIN=""; fi
[ -d "$HOME/.pi/agent" ] && AVAIL+=(pi)
# dsh is detected by its home, not by a binary. It runs as a node web server
# launched through a package manager, so there is no process signature (its
# descriptor sets `scannable = false`) and often no `dsh` on PATH — but
# `$DSH_HOME`, default `~/.dsh`, is where it keeps profiles and sessions.
DSH_HOME_DIR="${DSH_HOME:-$HOME/.dsh}"
DSH_BIN="$(find_tool dsh || true)"
[ -d "$DSH_HOME_DIR" ] && AVAIL+=(dsh)

describe() {
    case "$1" in
        claude) echo "Claude Code plugin — hooks, /floonet:inbox, and the floo_* MCP tools" ;;
        codex)  echo "codex plugin — hooks and the floo_* MCP tools" ;;
        pi)     echo "pi extension — native floo_* tools, no shelling out" ;;
        dsh)    echo "dsh plugin — registration, presence and wake for a browser session" ;;
    esac
}

WANT=()
if [ -n "$ONLY" ]; then
    # Explicit beats detected, and a name that is not available is an ERROR
    # rather than a silent omission: `--only codex` on a machine without codex
    # means the caller believes something false, and finishing quietly would let
    # them keep believing it.
    IFS=, read -r -a WANT <<< "$ONLY"
    for w in "${WANT[@]}"; do
        printf '%s\n' "${AVAIL[@]}" | grep -qx "$w" || {
            echo "--only names '$w', which is not available here." >&2
            echo "  available: ${AVAIL[*]}" >&2
            exit 1
        }
    done
elif [ "$INTERACTIVE" = "0" ]; then
    WANT=("${AVAIL[@]}")
    echo "installing every integration found: ${WANT[*]}  (--only to narrow)"
else
    echo
    echo "floonet installs into the agents you already run."
    echo
    for a in "${AVAIL[@]}"; do printf '  %-8s %s\n' "$a" "$(describe "$a")"; done
    echo
    echo "  Always installed: the fl/fld binaries, the fld background service, and"
    echo "  the menu bar panel. Those are floonet itself — without the service"
    echo "  nothing is discovered, and the panel is where a person sees it."
    echo
    printf 'Install all of the above? [Y/n/c=choose] '
    read -r ans || ans=""
    case "$ans" in
        [Nn]*) WANT=() ;;
        [Cc]*)
            for a in "${AVAIL[@]}"; do
                printf '  %-8s? [Y/n] ' "$a"
                read -r one || one=""
                case "$one" in [Nn]*) ;; *) WANT+=("$a") ;; esac
            done
            ;;
        *) WANT=("${AVAIL[@]}") ;;
    esac
    echo
    if [ ${#WANT[@]} -eq 0 ]; then
        echo "no integrations selected — floonet will run, but no agent will be able"
        echo "to reach it. Re-run install.sh to add them later."
    else
        echo "selected: ${WANT[*]}"
    fi
    echo
fi

wants() { printf '%s\n' "${WANT[@]+"${WANT[@]}"}" | grep -qx "$1"; }

# The skill, once, in the root that pi, codex and dsh all read.
#
# `~/.agents/skills` is not floonet's convention: codex documents it as its
# User scope, dsh scans it for compatible skills, and pi reads it alongside its
# own root. The extension, plugin or hook is what gives a session the floo_*
# tools; this is what tells the model when to reach for them. Tools without the
# skill are present but undiscoverable.
#
# Installed by each runtime that needs it rather than offered as its own menu
# entry. A skill is not a runtime, and listing it beside three of them asks a
# person to decide something they have no way to reason about. Claude Code
# scans personal, project and plugin roots only, and gets the same file through
# the plugin, so it never calls this.
#
# One file, not one per harness: a per-harness copy is the obvious next move
# and the wrong one — the last two shared 12 lines out of ~120 while claiming
# to say the same thing. `there_is_exactly_one_skill_and_it_matches_the_agent_skills_spec`
# is the gate that keeps it single.
#
# Idempotent and quiet after the first call: three branches may want it, and
# saying so three times is noise.
SHARED_SKILL_DONE=0
install_shared_skill() {
    if [ "$SHARED_SKILL_DONE" = "1" ]; then return 0; fi
    mkdir -p "$HOME/.agents/skills/floonet"
    install -m 644 "$REPO/plugin/skills/floonet/SKILL.md" \
        "$HOME/.agents/skills/floonet/SKILL.md"
    echo "floonet skill installed to ~/.agents/skills"
    SHARED_SKILL_DONE=1
}

# Asked before anything is touched: validation is cheap and must come before
# the expensive side effects, and a person can answer the questions and then
# leave while the build runs.

# ── Gate: refuse to install a commit CI has not passed ────────────────────────
#
# This script is floonet's deploy — the moment code becomes running software
# on a machine. There is no server to deploy to and no branch protection, so
# this check is the seam where a CI result becomes load-bearing.
#
# Three states, deliberately different:
#   clean + pushed  → require a green CI run for this commit.
#   dirty or local  → warn only. Refusing would make the script useless for
#                     the person developing floonet; the message says CI has
#                     never seen this build.
#   no gh / offline → warn only. A missing tool must not become a silent skip
#                     that looks like a pass.
#
# TP_SKIP_CI_GATE=1 overrides. An override that has to be typed is a decision;
# one that happens by default is an accident.
#
# Scope: this protects whoever builds from a checkout. A downstream user of a
# release usually has no `gh` and no reason to trust an API answer over the
# source they hold; what protects them is signed release artifacts, which is
# a different check from this one.
gate_ci() {
    [ "${TP_SKIP_CI_GATE:-}" = "1" ] && { echo "  CI gate: SKIPPED (TP_SKIP_CI_GATE=1)"; return 0; }

    # Every check below asks about a git checkout: is it dirty, is the commit
    # pushed, did CI pass for that sha. An unpacked release archive has no
    # commit — its provenance is the release it was downloaded from, which is a
    # different question this gate cannot answer. Saying nothing beats reporting
    # a missing answer to a question that was not asked.
    if [ "$PREBUILT" = "1" ]; then
        return 0
    fi

    if ! command -v gh >/dev/null 2>&1; then
        # Not "installing unverified": that phrasing implies a check that
        # should have happened and did not, and on the public repository no
        # check was ever going to run.
        echo "  CI gate: gh not installed — nothing to check against."
        return 0
    fi
    if [ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ]; then
        echo "  CI gate: working tree is dirty — installing a build CI has never seen."
        return 0
    fi

    local sha
    sha="$(git -C "$REPO" rev-parse HEAD 2>/dev/null)" || return 0
    if ! git -C "$REPO" branch -r --contains "$sha" >/dev/null 2>&1 \
       || [ -z "$(git -C "$REPO" branch -r --contains "$sha" 2>/dev/null)" ]; then
        echo "  CI gate: $sha is not pushed — installing a build CI has never seen."
        return 0
    fi

    # Derived from origin, never hardcoded: a fork must be checked against its
    # OWN CI. Hardcoding upstream would tell a fork's users that upstream is
    # green while installing the fork's code, which is worse than not checking.
    local slug conclusion
    slug="$(git -C "$REPO" remote get-url origin 2>/dev/null \
        | sed -E 's#^git@github\.com:#https://github.com/#; s#\.git$##' \
        | sed -E 's#^https://github\.com/##')"
    # owner/repo, and nothing that survived the sed untouched. A bare `*/*`
    # test is not enough: a GitLab https URL and a Bitbucket ssh URL both
    # keep a slash.
    case "$slug" in
        *://*|*@*|*:*|*" "*|*/*/*|*/) slug="" ;;  # kept a scheme, host or extra path
        */*) ;;                                    # owner/repo — the only shape we accept
        *) slug="" ;;
    esac
    if [ -z "$slug" ]; then
        echo "  CI gate: origin is not a GitHub repo — cannot check."
        return 0
    fi

    conclusion="$(gh run list --repo "$slug" --commit "$sha" \
        --workflow=ci.yml --limit 1 --json conclusion --jq '.[0].conclusion' 2>/dev/null)"

    case "$conclusion" in
        success)
            echo "  CI gate: green for ${sha:0:7}" ;;
        "")
            # Not "unverified": in the published repository nothing runs on a
            # push — the only workflow there builds release binaries from a
            # tag — so an outside install would read a normal state as a
            # warning.
            echo "  CI gate: no CI run for ${sha:0:7} — nothing to check against." ;;
        *)
            echo "" >&2
            echo "  CI gate: REFUSING — ci.yml concluded '$conclusion' for ${sha:0:7}." >&2
            echo "  https://github.com/$slug/actions" >&2
            echo "  Override with TP_SKIP_CI_GATE=1 if you know why." >&2
            exit 1 ;;
    esac
}
gate_ci

# An unpacked release archive is already built; the CI gate above is about a
# source checkout's provenance and has nothing to say about a downloaded
# tarball, whose provenance is the release it came from.
if [ "$PREBUILT" = "1" ]; then
    echo "using the binaries shipped in this archive (no build needed)"
    SRC_BIN="$REPO"
else
if reason="$(needs_build)"; then
    echo "building release binaries… ($reason)"
# cargo-auditable embeds the dependency tree into the binary, so an installed
# fl can be audited later with `cargo audit bin ~/.local/bin/fl`. Degrades to a
# plain build rather than forcing a toolchain install on every user; the note
# says what they are giving up.
if "$CARGO_BIN" auditable --version >/dev/null 2>&1; then
    "$CARGO_BIN" auditable build --release --manifest-path "$REPO/Cargo.toml" -p fl
else
    echo "  note: cargo-auditable not found — building without an embedded SBOM."
    echo "        \`cargo install cargo-auditable\` to make installed binaries auditable."
    "$CARGO_BIN" build --release --manifest-path "$REPO/Cargo.toml" -p fl
fi
else
    # Say which build is being installed: the risk of not compiling is
    # installing something older than you think, and a date is what lets that
    # be noticed at a glance.
    echo "installing the existing build from target/release ($(date -r "$SRC_BIN/fl" '+%Y-%m-%d %H:%M'))"
    echo "  sources are no newer than it — \`--build\` to compile anyway"
fi
fi

mkdir -p "$BIN_DIR" "$HOME/.teleport" "$HOME/Library/LaunchAgents"
# Install by atomic rename, not by writing over the destination.
#
# `install`/`cp` open the target with O_TRUNC, which rewrites the inode a
# running process has mapped; macOS then fails the code-signature check on the
# next page fault and kills it. A rename gives the new binary a fresh inode, so
# a running fld keeps its own until it exits and launchd only ever starts a
# complete file. The signature needs no help: the linker ad-hoc signs the
# binary and both cp and rename preserve it.
#
# Keep the one it replaces. Atomic is not recoverable, and `fld` is a
# LaunchAgent: a version that crashes on startup is restarted by launchd
# forever while the operator has nothing to put back. One generation, not an
# archive: two answers "the new one is broken, undo", while a directory of old
# builds is uncollected garbage and a decision made under pressure.
for b in fl fld; do
    install -m 755 "$SRC_BIN/$b" "$BIN_DIR/.$b.new"
    # cp, not mv: the running daemon has this inode mapped, and the point is to
    # copy the bytes aside, not to move the file out from under it.
    [ -f "$BIN_DIR/$b" ] && cp -p "$BIN_DIR/$b" "$BIN_DIR/.$b.prev"
    mv -f "$BIN_DIR/.$b.new" "$BIN_DIR/$b"
done

# Runtime descriptors are not copied: the binary carries them (`include_str!`,
# see decl.rs::EMBEDDED), and a file of the same id in ~/.teleport/runtimes.d
# is for user overrides only. A copied descriptor outlives the binary that
# matched it and silently overrides newer embedded text.
#
# Clean up what past installs left behind — but only copies byte-identical to
# what this build embeds, where removal is provably a no-op. A file that
# differs is customized or stale, and content cannot say which: it stays, it
# wins (that is the override contract), and `fl version` names it.
mkdir -p "$HOME/.teleport/runtimes.d"
for f in "$REPO/install/runtimes.d/"*.toml; do
    dst="$HOME/.teleport/runtimes.d/$(basename "$f")"
    if [ -f "$dst" ] && cmp -s "$f" "$dst"; then
        rm "$dst"
        echo "  removed $dst — identical to the descriptor embedded in this build"
    elif [ -f "$dst" ]; then
        echo "  kept $dst — differs from the embedded descriptor (customized or stale; see \`fl version\`)"
    fi
done

# Write beside it, then rename — the same discipline as the binaries above.
# `cmd > "$PLIST"` truncates the destination before the command runs, so a
# failure in the pipeline leaves a zero-byte plist, which `launchctl bootstrap`
# rejects with an I/O error that names neither the file nor the cause. With a
# rename the old plist stays complete until a whole new one exists, so a
# failed install leaves a working daemon.
sed -e "s|__USER__|$HOME|g" -e "s|__BIN__|$BIN_DIR/fld|g" \
    "$REPO/install/$LABEL.plist" > "$PLIST.new"
mv -f "$PLIST.new" "$PLIST"

# Starting the agent lives in its own file so a test can drive it with a
# stand-in launchctl; see install/start-agent.sh for why each step is there.
# Linted as a file of its own; followed from here only when both are given.
# shellcheck disable=SC1091
. "$REPO/install/start-agent.sh"
if start_agent "$LABEL" "$PLIST"; then
    TPD_STARTED=1
else
    case $? in
        2) TPD_STARTED=0 ;;
        *) exit 1 ;;
    esac
fi

# Claude Code, through its own package manager rather than by hand. `plugin/`
# declares the SessionStart/SessionEnd hooks, the /floonet:inbox command, the
# skill and the MCP server; installing the plugin delivers all four at once and
# `claude plugin uninstall` removes them again without this script's help. The
# MCP entry in particular lives in the plugin manifest and cannot be expressed
# by copying a file.
#
# Guarded like the pi block below: a machine using only codex has no `claude`
# binary. Not fatal if it fails — but said out loud, because without the hooks
# a session cannot be woken, and that is the half of floonet people notice
# missing.
if wants claude && [ -n "$CLAUDE_BIN" ]; then
    # add and install are idempotent (both exit 0 saying "already"); the two
    # updates are what a re-run is for. A marketplace sourced from a directory
    # is snapshotted at install time, so edits to this repo do not reach the
    # cache until it is refreshed.
    if "$CLAUDE_BIN" plugin marketplace add "$REPO" >/dev/null 2>&1 \
       && "$CLAUDE_BIN" plugin install floonet@floonet --scope user >/dev/null 2>&1 \
       && "$CLAUDE_BIN" plugin marketplace update floonet >/dev/null 2>&1 \
       && "$CLAUDE_BIN" plugin update floonet@floonet >/dev/null 2>&1; then
        echo "Claude Code plugin installed — hooks, /floonet:inbox, skill and MCP tools"
    else
        echo "  warning: could not install the Claude Code plugin." >&2
        echo "           fl and fld work; Claude Code sessions will not be" >&2
        echo "           registerable or reachable until it is installed." >&2
        echo "           Retry:  claude plugin marketplace add $REPO" >&2
        echo "                   claude plugin install floonet@floonet" >&2
    fi
elif [ -z "$CLAUDE_BIN" ]; then
    # Absent is worth saying; DESELECTED is not — the person just answered that
    # question and does not need it repeated back as a warning.
    echo "  note: claude not found — skipping the Claude Code plugin."
fi

# Same, for the pi agent harness — only if pi is installed.
if wants pi && [[ -d "$HOME/.pi/agent" ]]; then
    mkdir -p "$HOME/.pi/agent/extensions"
    install -m 644 "$REPO/integrations/pi/floonet.ts" "$HOME/.pi/agent/extensions/floonet.ts"
    # The extension registers the floo_* tools; the skill is what says when to
    # reach for them. pi scans `~/.agents/skills` alongside its own root, and
    # that shared root is pi's only source for it — unlike claude and codex,
    # nothing else delivers it here.
    install_shared_skill
    echo "pi extension installed — run /reload in any already-running pi session to pick it up"
fi

# codex, through its package manager, for the same reason Claude Code goes
# through `claude plugin install`: `codex plugin add` and `codex mcp add` are
# the interfaces, and hand-editing ~/.codex/config.toml is the documented
# fallback. A hand-edited config names a binary path nothing knows to update
# when this install replaces it; a package manager knows.
#
# The plugin carries skills, hooks and the MCP server together
# (plugin/.codex-plugin/plugin.json). Codex reads the marketplace from
# .agents/plugins/marketplace.json; the older .claude-plugin/marketplace.json
# is legacy-compatible and both resolve to the same plugin.
#
# The failure output is kept, not discarded: `codex plugin marketplace add`
# fails for reasons a user can act on — a sandbox denying ~/.codex, a
# malformed manifest — and none of them are guessable from "could not
# install".
if wants codex && [ -n "$CODEX_BIN" ]; then
    # The marketplace is registered by path, and an upgrade is usually unpacked
    # somewhere new. codex then refuses the add — "marketplace 'floonet' is
    # already added from a different source" — and the plugin stays on the old
    # version for good; measured on a machine upgraded from 0.2.1, whose codex
    # kept loading the old, unparseable hooks.json. Claude Code switches the
    # source itself. Removing first makes codex do the same; `plugin add` below
    # reinstalls, and a first install has nothing to remove.
    "$CODEX_BIN" plugin marketplace remove floonet >/dev/null 2>&1 || true
    # Success is the exit status, not an empty stderr: codex prints warnings
    # to stderr on successful runs, and judging by emptiness would turn each
    # one into a failed install.
    if codex_err=$("$CODEX_BIN" plugin marketplace add "$REPO" 2>&1 >/dev/null) \
        && codex_err=$("$CODEX_BIN" plugin add floonet@floonet 2>&1 >/dev/null); then
        echo "codex plugin installed — skill and MCP tools; hooks are"
        echo "  auto-discovered and trust-gated, so the first codex session may"
        echo "  prompt before they run (verify: fl live shows [hook] beside it)"
    else
        # The plugin carries the skill, so this is the only branch that needs
        # the shared root: without the plugin it is the one place left a codex
        # session can still find it.
        install_shared_skill
        echo "  note: could not install the codex plugin:" >&2
        printf '        %s\n' "$codex_err" | head -3 >&2
        echo "        fl and fld work; codex sessions reach floonet through the" >&2
        echo "        shared skill only. Retry:" >&2
        echo "          codex plugin marketplace add $REPO" >&2
        echo "          codex plugin add floonet@floonet" >&2
    fi
fi

# dsh, through its own plugin installer, for the same reason Claude Code and
# codex do: dsh is a Cordis composition where everything is a plugin, and
# `dsh plugin` forwards to pnpm inside the profile and maintains that profile's
# manifest. Its own tutorial is explicit that a profile manifest is never
# written by hand, and the first attempt here did exactly that — copying the
# package into `profiles/web/node_modules/` and editing the user's own
# `cordis.patch.yml`, which is the one layer that belongs to them.
#
# The bundle is `integrations/dsh`: its package.json declares
# `dsh.bundle.patch`, pointing at the cordis.patch.yml that inserts floonet's
# row. A package WITHOUT `dsh.bundle` still installs and activates nothing,
# which is a silent no-op worth knowing about.
#
# Installed from the checkout path, not by name: `@floonet/dsh` is what a
# published package would be called, and this is a checkout.
#
# The binary is frequently absent on a machine that runs dsh — it is started as
# `pnpm dsh web` from inside a checkout — so a missing `dsh` is not a failure
# here. The command is printed instead of guessed, because the profile name is
# the user's composition and `web` is only the common default.
if wants dsh; then
    install_shared_skill
    if [ -n "$DSH_BIN" ] \
       && "$DSH_BIN" plugin --profile web add "$REPO/integrations/dsh" >/dev/null 2>&1; then
        echo "dsh plugin installed into the 'web' profile — restart dsh to load it"
    else
        echo "  note: the dsh plugin was not installed automatically."
        echo "        dsh sessions stay searchable either way; registration,"
        echo "        presence and wake are what need the plugin. Run this from"
        echo "        your dsh checkout, naming the profile you actually use:"
        echo "          dsh plugin --profile web add $REPO/integrations/dsh"
    fi
fi

# Floonet Panel — the menu bar GUI, and part of the product rather than an
# extra: it is how a person sees which sessions are live, which machines are
# paired, and whether the peer port is open.
#
# Three sources, in the order a machine is likely to have them:
#
#   1. a prebuilt bundle beside this script — the release archive, where the
#      .app is built in CI as a universal binary and no toolchain is needed;
#   2. Swift sources plus a toolchain — a git clone;
#   3. neither, which is reported rather than skipped in silence: the only
#      symptom of a missing panel is a menu bar with no icon and no way to
#      learn why.
#
# A failure here is reported. "Optional" means floonet still works, not that
# a failure goes unmentioned.
PANEL_OK=0
if [ -d "$REPO/FloonetPanel.app" ]; then
    # Placed here rather than by panel/Makefile: a release archive ships the
    # bundle without the sources, so there is no Makefile to call. Atomic
    # rename, as for the binaries: writing over a running bundle file-by-file
    # fails its code signature on the next page fault.
    if rm -rf "/Applications/.FloonetPanel.app.new" \
       && cp -R "$REPO/FloonetPanel.app" "/Applications/.FloonetPanel.app.new"; then
        if pgrep -x FloonetPanel >/dev/null 2>&1; then
            echo "  quitting the running FloonetPanel first"
            osascript -e 'tell application "FloonetPanel" to quit' >/dev/null 2>&1 \
                || pkill -x FloonetPanel || true
            sleep 1
        fi
        rm -rf "/Applications/FloonetPanel.app"
        if mv "/Applications/.FloonetPanel.app.new" "/Applications/FloonetPanel.app"; then
            PANEL_OK=1
        fi
    fi
    [ "$PANEL_OK" = "1" ] || {
        echo "WARNING: could not place the prebuilt panel into /Applications." >&2
        echo "         Everything else is installed." >&2
        rm -rf "/Applications/.FloonetPanel.app.new"
    }
elif [ -d "$REPO/panel" ] && command -v swift >/dev/null 2>&1; then
    if (cd "$REPO/panel" && make install); then
        PANEL_OK=1
    fi
elif [ -d "$REPO/panel" ]; then
    echo "WARNING: the menu bar panel needs a Swift toolchain to build from source," >&2
    echo "         and this machine has none. Everything else is installed." >&2
    echo "         Install Xcode Command Line Tools (xcode-select --install) and re-run," >&2
    echo "         or use a release archive, which ships the panel prebuilt." >&2
else
    echo "WARNING: no menu bar panel in this package — neither a prebuilt" >&2
    echo "         FloonetPanel.app nor panel/ sources are beside install.sh." >&2
    echo "         Everything else is installed. This is a packaging fault, not" >&2
    echo "         a choice: report it." >&2
fi
if [ "$PANEL_OK" = "1" ]; then
    echo "Floonet Panel installed to /Applications"

    # It is a plain .app, NOT a LaunchAgent like fld — nothing starts it at
    # login unless it is registered, so after a reboot the icon is simply
    # gone and the app looks broken rather than absent. Asked once, never
    # assumed: adding a login item changes the user's session, and this
    # script should not do that quietly.
    if osascript -e 'tell application "System Events" to get name of every login item' 2>/dev/null | grep -q FloonetPanel; then
        echo "  (already a login item)"
    elif [ -t 0 ]; then
        printf "  Start Floonet Panel automatically at login? [y/N] "
        read -r reply
        case "$reply" in
            [yY]*)
                osascript -e 'tell application "System Events" to make login item at end with properties {path:"/Applications/FloonetPanel.app", hidden:false}' >/dev/null \
                    && echo "  added to login items" \
                    || echo "  could not add the login item — System Settings > General > Login Items"
                ;;
            *) echo "  skipped — add it later in System Settings > General > Login Items" ;;
        esac
    else
        echo "  not running interactively; add it in System Settings > General > Login Items to start it at login"
    fi

    open -a /Applications/FloonetPanel.app 2>/dev/null || true
    fi

echo
echo "What takes effect when:"
echo "  fl binary → CLI and pi           : now"
echo "  fl binary → Claude Code MCP tools: NEW Claude Code session (the MCP"
echo "                                     server keeps the binary it started with)"
echo "  skills / fl.md / pi extension    : /reload-skills here, /reload in pi"
echo "  runtimes.d/*.toml                : next fl/fld invocation (read per-run)"
# Not a fixed string: the line above it can be the skip branch, and a summary
# that says "restarted above" when nothing was restarted is the kind of quiet
# untruth this installer's output is supposed to be free of.
if [ "${TPD_STARTED:-0}" = "1" ]; then
    echo "  fld                              : restarted above"
else
    echo "  fld                              : NOT running — see the note above"
fi
echo
echo "  $BIN_DIR/fl live      # agent sessions running right now"
echo "  $BIN_DIR/fl id        # this machine's fingerprint, for pairing"
echo
# ── PATH ─────────────────────────────────────────────────────────────────────
#
# `~/.local/bin` is not on the default PATH on macOS (/etc/paths lists only
# /usr/local/bin, /usr/bin, /bin, /usr/sbin, /sbin). Everything floonet runs
# itself is immune — hooks, the LaunchAgent, the pi extension, the dsh plugin
# and the MCP entry name the binary absolutely. What breaks is the person
# typing `fl live`, who has no reason to suspect PATH.
#
# So: check, and offer. Editing someone's shell profile unasked is not ours to
# do; leaving them with a binary they cannot run is not much better. A
# terminal gets the offer, everything else gets the sentence.
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        # Which file. zsh is macOS's default login shell and reads .zprofile for
        # login shells; bash reads .bash_profile. Chosen from $SHELL rather than
        # from what exists, because creating .zprofile on a bash user's machine
        # is litter that will never be read.
        case "${SHELL##*/}" in
            zsh)  PROFILE="$HOME/.zprofile" ;;
            bash) PROFILE="$HOME/.bash_profile" ;;
            *)    PROFILE="" ;;
        esac
        LINE="export PATH=\"\$HOME/.local/bin:\$PATH\""
        if [ "$INTERACTIVE" = "1" ] && [ -n "$PROFILE" ]; then
            echo
            echo "$BIN_DIR is not on your PATH, so \`fl\` will not be found."
            echo "  (floonet's own hooks and services are unaffected — they use the"
            echo "   full path. This is only about typing \`fl\` yourself.)"
            echo
            printf 'Add it to %s? [Y/n] ' "${PROFILE/#$HOME/~}"
            read -r a || a=""
            case "$a" in
                [Nn]*) echo "  not added. To do it later:  echo '$LINE' >> $PROFILE" ;;
                *)
                    # Append, never rewrite, and check first: running the
                    # installer twice must not leave two copies.
                    if [ -f "$PROFILE" ] && grep -qF '.local/bin' "$PROFILE"; then
                        echo "  $PROFILE already mentions .local/bin — left alone"
                    else
                        printf '\n# added by floonet install.sh\n%s\n' "$LINE" >> "$PROFILE"
                        echo "  added to $PROFILE — open a new terminal, or: source $PROFILE"
                    fi
                    ;;
            esac
        else
            echo
            echo "note: $BIN_DIR is not on your PATH, so \`fl\` will not be found."
            echo "      floonet's hooks and services are unaffected (they use the full"
            echo "      path); this is only about typing \`fl\` yourself. To fix:"
            echo "        echo '$LINE' >> ${PROFILE:-your shell profile}"
        fi
        ;;
esac
