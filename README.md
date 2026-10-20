# Floonet

Let your coding agents read and message each other's terminals.

You have Claude Code open on one project, Pi on another, Codex on a third. The
agent in this terminal needs to know what the agent in that one just found — or
needs it to do something. Today the only bridge between two terminals is you,
copying output from one and pasting it into the other.

Floonet gives each agent a skill and a set of tools for the other terminals:
**read** what any session discussed or is discussing right now (its text, and
its `thinking` if you ask), and **write** to a live one — drop a task in its
mailbox and wake it, and it replies when done. The same tools reach sessions on
a trusted Mac on your LAN. Every session that ever ran on the machine is
searchable too, including the ones whose transcripts the runtime has since
deleted, so "what did I work out last week in the other tab" is a query, not a
re-explanation.

Rust + SQLite + a LaunchAgent, macOS. Reads **Claude Code**, **Pi** and
**Codex** today, and reaches **dsh** sessions; a new runtime's transcripts are
taught to it with a TOML file rather than code.

You talk to your agent the way you already do. The tools are its, not yours:

> **You, in Claude Code on `~/dev/api`:** what did the Pi session on `~/dev/web`
> decide about the auth header?
>
> *Claude Code calls `floo_sessions`, then `floo_turns` on the one it finds, reads
> that session's last hour, and tells you.*
>
> **You:** tell it to use the v2 header name from now on.
>
> *Claude Code calls `floo_ask`. In the other terminal, the Pi session wakes, reads
> its inbox, makes the change, and replies. Claude Code relays the reply.*

The same works from Pi and Codex, and toward a session on a trusted Mac on your
LAN. There is a CLI underneath (`fl`) — it is what the tools call, and the one
time you use it yourself is to pair two machines, a step that is deliberately a
person's and not a tool's. It is documented [further down](#the-fl-cli).


## What it does

| Capability | What you get |
|---|---|
| **Search** | Query every Claude Code, Pi, Codex and dsh transcript on this machine — or on a trusted peer — and get *coordinates + excerpt*, not whole dumps. `thinking` is searchable, opt-in. |
| **Read** | Pull a real conversation back: one session, a time window, or one specific day. Bounded, and it tells you when it truncated and how to page. |
| **Reach** | `fl ask` enqueues a message into a *live* session's mailbox and wakes it — delegate work, not just ask questions. Content never crosses a pane as keystrokes; only a fixed `/floonet:inbox` control string does. The target does the work and `fl reply`s with what it did. |
| **Federate** | Probe a named host for a floonet daemon, pair with explicit human approval + out-of-band fingerprint comparison, then query them. Requests are signed (RFC 9421) over TLS. |

### The honest capability matrix

Not everything is shipped. This is what a fresh install actually does:

| Feature | Same machine | Across machines (LAN/VPN) |
|---|---|---|
| Search sessions + turns | ✅ scan-based, sub-second when scoped | ✅ fan-out to trusted peers |
| Search `thinking` | ✅ opt-in (`--include-thinking`) | ✅ opt-in |
| List live sessions | ✅ `fld` active scan, authoritative | ❌ own machine only |
| Poke / message a live session | ✅ `fl ask` → tmux/iTerm2 wake | ❌ **not shipped** — designed, not wired |
| Type raw keystrokes | ✅ `fl type` — no safety gate, CLI-only | ❌ |

Per runtime:

All four are read through a shipped TOML descriptor — there is no hand-written
adapter left for any of them, so "supported" and "has a descriptor" mean the
same thing here.

| Runtime | Search history | Live + pokeable | Can *use* floonet | How `install.sh` wires it |
|---|---|---|---|---|
| Claude Code | ✅ | ✅ hooks | ✅ MCP tools + skill | `claude plugin install` — hooks, `/floonet:inbox`, skill, MCP server |
| Pi | ✅ | ✅ extension | ✅ native tools + skill | copies the extension into `~/.pi/agent/extensions` |
| codex | ✅ | ✅ hooks | ✅ MCP tools + skill | `codex plugin add` — MCP tools and skill; you approve the hooks once with `/hooks` |
| dsh | ✅ | ✅ plugin | ✅ native tools + skill | `dsh plugin add` — the Cordis plugin in `integrations/dsh/`, plus the shared skill |

On a terminal, `install.sh` detects which of these you have and asks which to
wire. In a pipe, a CI job or with `--yes` it installs whatever it detects;
`--only claude,pi` names a subset without a prompt. The binaries, the daemon
and the menu bar panel are never optional, because nothing works without them.

**One skill document, in a directory three of the four agree to read.**
`~/.agents/skills` is not floonet's convention: codex documents it as its User
scope, dsh scans it for compatible skills, and pi reads it alongside its own.
Claude Code scans only personal, project and plugin roots, and gets the same
file through the plugin. It is never a separate thing to install — each
runtime that needs it puts it there, because a skill is not a harness and
asking someone to choose one is asking a question they cannot answer.

**The tool surface is the same everywhere, with one deliberate gap.** Claude
Code, codex and dsh expose fourteen `floo_*` tools: search, sessions, turns,
live, peers, discover, three pairing tools, ask, note, reply, inbox and ack. Pi
exposes thirteen — no `floo_inbox`, because its extension drains the mailbox
through a command of its own when a wake arrives. Pairing *approval* is not a
tool on any surface; trust is granted by a person at the `fl` CLI.

**`/floonet:inbox` exists only where a slash command can.** It is the briefing
a woken Claude Code session reads before acting on a mailbox that came from
somewhere else. codex plugins have no command mechanism to hang it on, and
dsh's plugin delivers the same briefing through its own wake path, so neither
is missing anything a message needs.

codex's session-start hook states the session id, so floonet is told it rather
than inferring it from the process tree. codex records trust against a hash of
the exact hook definition, so the hooks run only after you approve them once.

**dsh transcripts are Zstandard, appended one frame per write**, and the two
message shapes sit at different paths, which is why a runtime descriptor can
declare a multi-frame source and per-entry content paths. What reading from
outside cannot reproduce is dsh's own resolution of which events are current
and which are shadowed, so floonet declares no compaction marker for dsh and
reports every dsh turn as `unknown` surface rather than `current`. A scan may
be wrong about supersession; it is never allowed to claim it is right.

All of them see each other: from a Pi session you can search what a Claude Code
session worked out, and the reverse.

Cross-machine federation today means **read**. Cross-machine **reach** — poking
a session on another Mac — is designed but not wired end to end. Don't expect it
from a fresh install.

## Install

Download the release for your Mac. No toolchain, no build.

```bash
# Apple silicon:  floonet-arm64     Intel:  floonet-x86_64
curl -sSLO https://github.com/agentmessier-ai/floonet/releases/latest/download/floonet-arm64.tar.gz
curl -sSLO https://github.com/agentmessier-ai/floonet/releases/latest/download/floonet-arm64.tar.gz.sha256
shasum -a 256 -c floonet-arm64.tar.gz.sha256
tar xzf floonet-arm64.tar.gz
cd floonet-arm64 && ./install/install.sh
```

The archive carries the binaries next to `install.sh`, which detects them and
skips the build entirely — the whole install takes well under a second.

<details>
<summary>From source, if you are changing the code</summary>

```bash
git clone https://github.com/agentmessier-ai/floonet.git
cd floonet
./install/install.sh
```

Needs a [Rust toolchain](https://rustup.rs) — rustup specifically, since
`rust-toolchain.toml` pins the version and only rustup reads it.

Budget for it. The release profile uses fat LTO, whose final link is
single-threaded: about a minute on Apple silicon and much longer on an older
Intel laptop. Downloading the release is the fix, which is why it is above.

</details>

Either way, `install.sh` puts `fl` and `fld` in `~/.local/bin`, registers
`io.teleport.tpd` as a LaunchAgent (runs on login, restarts on crash, owns the
SQLite store at `~/.teleport/teleport.db`), and wires the integrations you
chose: the Claude Code plugin through `claude plugin install`, the codex plugin
through `codex plugin add`, the dsh plugin through `dsh plugin add`, and the Pi
extension. `~/.local/bin` is not on the default macOS `PATH`; the script offers
to add it to your shell profile. Re-running is safe and is how
you pick up changes: a directory-sourced plugin marketplace is snapshotted at
install time, so the script refreshes it every run.

A harness it does not find on `PATH` is simply not offered; floonet still
works, and that harness's sessions can be searched but not woken until its
integration is installed.

The menu bar panel is source-only. `install.sh` builds it when it finds
`panel/` next to itself and a Swift toolchain on PATH, which means the clone
above and not the release archive — the archive ships built binaries, not Swift
sources, so a Swift toolchain does nothing for it.

Over ssh it installs everything but does not start `fld`, and says so: a
LaunchAgent needs a GUI login session to bootstrap into, and there isn't one.
The plist is in place, so launchd starts it at the next login on that machine's
own screen.

Requirements: macOS, and `~/.claude` for Claude Code sessions. Optional: Pi,
tmux, or iTerm2 for the respective reach paths.

**Linux** is not supported, and the reason is narrower than that sounds. The
workspace has no platform-specific code — no `cfg(target_os)`, no macOS-only
dependencies — and the suite passes on x86_64 Linux. What has never been run
there is the *reach* half: waking a session, injecting into a terminal, and a
daemon that assumes launchd rather than systemd. Search and indexing would
likely work. Nobody has checked, so the answer stays "no" rather than
"probably".

### What takes effect when

Getting this wrong looks exactly like a change that silently didn't happen:

| Changed | Takes effect |
|---|---|
| `fl` binary → CLI, and Pi (which re-execs `fl` per call) | immediately |
| `fl` binary → **Claude Code's MCP tools** | **a new Claude Code session** — the MCP server is a long-lived child spawned at session start, so it keeps the binary it was launched with. `/reload-plugins` does *not* restart it. |
| skills / `fl.md` / the Pi extension | `/reload-skills` in Claude Code, `/reload` in a running Pi session |
| `~/.teleport/runtimes.d/*.toml` | next `fl`/`fld` invocation — read per run |
| `fld` | `install.sh` restarts it |

Uninstall: `./install/install.sh --uninstall` removes the LaunchAgent, the
panel and every integration, then reports what `~/.teleport` holds (sessions,
messages, pairings) and asks separately whether to remove the binaries and the
data. `--uninstall --purge` removes the data without asking.

## What floonet keeps, and who can read it

**Your conversations are not stored here.** floonet reads each runtime's own
transcript, on the query that asks for it, and copies nothing. `~/.teleport/`
holds the mailbox (messages between your agents), which sessions are live right
now, the machines you have paired with, and this machine's identity key. It does
not hold a second copy of anything you said to an agent.

That is a change from earlier versions, which did keep one. Retention is now
entirely the runtime's: Claude Code deletes its transcripts after about a month
and floonet then has nothing to search for that period either. floonet neither
extends what your agents keep nor shortens it.

**How to make it forget.** Delete the runtime's own transcript and it is gone
from floonet too — there is no second copy to find. `rm -rf ~/.teleport` removes
the mailbox, the identity key and the pairings. Uninstall does not do either for
you.

**Nothing listens until you say so.** A fresh install opens no port. floonet is
local — it reads files, writes one database, and types into terminals on this
machine — and the peer listener is the one thing that makes it reachable from
outside, so it is off by default. `fl listen on`, or the panel's switch, turns
it on; `fl listen` says which it is. Pairing works either way, because `fl pair`
is local and human-approved.

**What a paired machine can read, once you do.** A peer you have approved can
search every transcript on this machine, over whatever history the runtimes
still hold, reasoning included when the query asks for it, and sees session
titles and working-directory paths. There is no owner-side scope: nothing here
excludes a folder, a period, or the reasoning from what a peer may search. The
control is pairing itself — pair only machines you would hand a keyboard, and
`fl pair revoke` when that changes.

**Where it does not go.** Nowhere else. The daemon talks only to peers you have
approved; nothing is sent to any third party, ever.

## Reading a conversation back

Three modes, because what you know going in differs:

| You know | Use |
|---|---|
| a phrase | `fl search "…"` → coordinates + excerpt (cheap) |
| nothing | `fl sessions --since 4h` → which sessions were active |
| a time | `fl turns --since 4h --folder …` → full text, no session id needed |
| a session | `fl turns <id>` → full text from the start |
| a cursor | `fl turns <id> --after-ts <ms>` → resume forward |

Both time bounds accept a duration (`4h`, `2d`), a local date (`2026-08-04`), a
wall clock (`2026-08-04T14:30`), or unix ms. `--until` is exclusive, so a day is
`--since 2026-08-04 --until 2026-08-05` and paging back terminates.

Reading by time reads **one** session — the most recent in the window. That is a
guess, so it is only made when something narrows it: give `--folder` (or a
session id), or `fl turns` lists the candidates and stops rather than answering
a different question than you asked. `fl sessions` is the cross-session view.

The two directions are not the same operation, and the difference is the whole
point:

```
--since 4h        a window. Drops the OLDEST when it overflows → keeps "just now"
                  page back with:  --until <earliest ts>

--after-ts <ms>   a cursor. Drops the NEWEST → keeps the beginning
                  page on with:    --after-ts <last ts>
```

A truncated read always says so and prints the command that continues it, in the
direction it actually read.

## The `fl` CLI

| Command | What it does |
|---|---|
| `fl search <query>` | Search sessions. `--since`/`--until`, `--folder`, `--include-thinking`, `--regex`, `--all` (query peers). |
| `fl sessions` | Sessions active in the window, most recent first. `--since`/`--until`, `--folder`. |
| `fl turns [session_id]` | Read turns. `--since`/`--until` for a window (session id optional), `--after-ts` to resume forward, `--include-thinking`. |
| `fl live` | Sessions running right now, reconciled by `fld`'s active scan — authoritative over hook registrations. |
| `fl ask <session_id> <msg>` | Enqueue into a session's mailbox and wake it. Stamps a return address so the target can answer. `--no-wake` parks it. |
| `fl note <session_id> <msg>` | Tell a session something that needs no answer. Wakes it, expects nothing back. |
| `fl reply <msg_id> <msg>` | Answer a message from your inbox, addressed from the original so it can't be misrouted. |
| `fl inbox` | Drain *this* session's mailbox — what `/floonet:inbox` triggers. `--pending` lists what was shown but never acked. |
| `fl ack <msg_id>` | Mark a message acted on. Reading and acting are separate facts; an unacked message stays recoverable. |
| `fl type <tty> <text>` | **Unsafe path.** Types raw text into a pane: no mailbox, no control string, no gate. CLI-only by design, never an MCP tool. |
| `fl listen [on\|off]` | Whether this machine accepts connections from peers. Off on a fresh install. |
| `fl id` | This machine's fingerprint — compare out of band when pairing. |
| `fl peers` / `fl pair` | Trust state; `pair request/list/approve/reject/revoke`. Approval is a person's, at this CLI, never a tool's. |
| `fl discover <host>` | Ask one host whether it runs a daemon (default port + a few neighbours). Read-only: answering is not trusting. |
| `fl mcp` | Run the MCP server over stdio. |
| `fl register` / `fl heartbeat` / `fl unregister` | Live-session registry, driven by hooks and extension events. |
| `fl version` | This binary's build and the running daemon's, which can differ until `fld` restarts. |
| `fl verify` / `fl backup` / `fl rollback` | Check the store for damage; copy it consistently while in use; put back the binaries the last install replaced. |

```bash
# The other terminal: what is it doing, and tell it something
fl live                                                # every agent session running right now
fl turns --since 30m --folder ~/dev/other-project      # read its last half hour
fl ask AAAA-…/pi/019fc929-… "which spec is authoritative?"   # it does the work and replies

# What was I doing this afternoon? (no session id needed — finding it IS the question)
fl turns --since 4h --folder ~/dev/myproject

# What happened on one specific day
fl sessions --since 2026-08-04 --until 2026-08-05                  # who was working then
fl search "oauth keychain" --since 2026-08-04 --until 2026-08-05   # where it was discussed
fl turns --since 2026-08-04 --until 2026-08-05 --folder ~/dev/myproject

# Where was this discussed, across every session on the machine
fl search "cache invalidation" --since 7d --include-thinking

# Reach another machine — name it; there is no LAN-wide browse
fl discover 192.0.2.42
fl pair request 192.0.2.42:47400
fl pair approve XXXX-XXXX-…
fl search "cache invalidation" --all                   # …and search it too
```

Every command reports coverage explicitly. A truncated or degraded scan is never
presented as an answer — because "no matches" and "I didn't finish looking" are
the same output otherwise, and only one of them means it never happened.

## From an agent

**Claude Code** and **codex** get MCP tools: `floo_search`, `floo_sessions`,
`floo_turns`, `floo_live`, `floo_ask`, `floo_note`, `floo_reply`,
`floo_inbox`, `floo_ack`, `floo_peers`, `floo_discover`,
`floo_pair_request`, `floo_pair_list`, `floo_pair_reject`. **Pi** and **dsh**
get the same set as native tools. A test asserts the surfaces expose the same
parameters.

A message carries a `kind`. `ask` expects a reply and `note` does not; both
wake the target. The envelope is open: a sender outside floonet — a build, a
review, a webhook — can deliver its own kind with a declared content type and
extension fields, and the receiving agent sees it as an event rather than as a
question it owes an answer to.

A skill teaches each agent what "carry over the conversation from my other Mac"
means step by step — and what *not* to do (never route around approval being a CLI-only step;
never silently drop a peer that didn't answer a `--all` search).

**Messaging is asynchronous with no completion callback.** `fl ask` returns as
soon as the message is queued and the target woken; the target's `fl reply` is
the only signal the sender ever gets. An agent that needs an answer should send
and end its turn — the reply arrives as a `/floonet:inbox` wake that resumes it —
rather than polling in a `sleep` loop.

### Sending code in a message

Agents mostly send each other prose *about* code, so a message body normally
contains backticks, `$` and `!`. In a double-quoted shell argument the shell
expands those **before floonet sees anything**, and the message is delivered in
full with a silently different body — the failure mode this project keeps
finding elsewhere, arriving through the shell instead.

It has happened here: a backtick-quoted phrase became a command substitution,
resolved to the empty string, and removed the subject of the sentence around it.
The send reported success, because from floonet's side it was one.

Use a quoted heredoc:

```bash
fl reply <id> "$(cat <<'EOF'
`backticks`, $VARS and !history all survive verbatim
EOF
)"
```

The quotes around `EOF` are the load-bearing part — a bare `<<EOF` still
expands. Single-quoting the whole argument also works, until the body contains
an apostrophe.

Agents calling the MCP or pi tools (`floo_ask`, `floo_reply`) pass the
body as a JSON string and are not affected; this is a shell-only hazard.

## Security model

**A fresh install opens no port.** floonet is local — it reads transcript files,
writes one database, and types into terminals on this machine. The peer listener
is off until you turn it on (`fl listen on`, or the panel's switch).

The reach path rests on one hard rule: **only a fixed control string is ever
typed into another session's pane** — real content stays in the mailbox, which
today only local processes (anything running as you) can write to, and no
network route to it exists at all. When cross-machine reach ships, the gate is
the peer you approved by hand — so pair only machines you'd hand a keyboard.

Identity, pairing, the signed peer protocol, the redaction funnel, and the
reach rate-limits — the full model, including what each guarantee is and
isn't: [SECURITY-MODEL.md](SECURITY-MODEL.md). Reporting a vulnerability:
[SECURITY.md](SECURITY.md).

## Architecture

Two binaries: `fld`, a LaunchAgent that owns the database, the process scan, and
— when you enable it — the LAN socket; and `fl`, a stateless CLI + MCP server.
No shared mutable state and no second copy of anything: a search reads each
runtime's own transcript on the query that asks for it, and cross-machine search
is fan-out read at query time. Runtimes are TOML configuration, not code.

The system carries several names — `floonet` as the product, `fl`/`fld` as
the binaries, `tp-*` as the crates, `floo_*` as the MCP tools. Which surface
uses which, and which strings are stable identifiers that must not be
"unified": [ARCHITECTURE.md § Names](ARCHITECTURE.md#names).

The diagram, the crate layout, and the reasoning behind each choice:
[ARCHITECTURE.md](ARCHITECTURE.md).

## The menu bar panel

`panel/` is a small SwiftUI `LSUIElement` app: human-readable **aliases** for
live sessions (keyed on `cwd`, so they survive `/clear` and restart), one-click
poke and focus, daemon health, and a recent-poke log. A read-only viewer and
launcher — `fld`'s scan stays the source of truth.

## Development

Build commands, local hooks, what CI enforces, and how to send a patch:
[CONTRIBUTING.md](CONTRIBUTING.md). The crate layout and the one design note
worth internalizing before touching code:
[ARCHITECTURE.md](ARCHITECTURE.md#the-crate-layout).

## Roadmap

- Cross-machine reach — designed; needs an `.app` + SMAppService +
  XPC path for daemon-initiated injection
- More runtimes: openclaw, hermes — the read side is a TOML descriptor now,
  and codex was the first one added that way
- Indexing tool *results*, where test failures and error output actually live —
  they're dropped today, and turning them on is a redaction decision first
- Ranked results: a scan returns file order, newest first, so `--limit` gives
  you the most RECENT matches rather than the best ones

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) — short on ceremony, long on the two or
three conventions here that a well-meaning patch tends to get wrong.

Security issues go to [`SECURITY.md`](SECURITY.md), not to the issue tracker.

## License

MIT — see [`LICENSE`](LICENSE).

Claude Code, Pi and Codex are names of their respective projects. Floonet
reads their files; it is not affiliated with any of them.
