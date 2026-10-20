# Architecture

```
  ~/.claude/…  ~/.pi/…  ~/.codex/…  ~/.dsh/…      the runtimes' own transcripts
       │            │        │          │          — floonet copies none of them
       └────────────┴───┬────┴──────────┘
                        │ read on the query that asks
                        ▼
                 adapter → redact → answer
                        ▲
                     ┌──┴────────────────── fld (LaunchAgent) ──────────┐
                     │  process scan · live_session · mailbox (SQLite)  │
                     │  net: axum HTTP · probe · pairing · peer fan-out │
                     └───────┬──────────────────────────────┬───────────┘
                             │ unix socket (panel)          │ :47400 — OFF by
                   ┌─────────┴────────┐             ┌───────┴──────┐  default
                   │ fl CLI · MCP ·   │             │ peer machine │
                   │ hooks · panel    │             └──────────────┘
                   └──────────────────┘
```

**The database holds no conversations.** It holds the mailbox, who is live, and
who this machine has paired with. A search reads the runtime's own transcript
files; nothing is copied in, so nothing can go stale, and floonet's retention is
whatever each runtime's is.

- **Two binaries.** `fld` is the resident daemon: it owns the database, the
  process scan, the local socket, and the injectors. `fl` is a stateless CLI
  and MCP server that opens, and migrates, the database on every invocation,
  including from hooks. That cost is accepted: the open sets a busy timeout,
  registration is an upsert, and the one race — two processes migrating a
  brand-new store at the same instant — is described in
  `plugin/hooks/README.md`.
- **SQLite, not a cluster.** There is no shared mutable state: every machine
  writes its own sessions and peers only read. Cross-machine search is fan-out
  read at query time, with no sync protocol, because a two-node consensus
  group is strictly worse than one node when one of them is a sleeping laptop.
- **Retrieval is a seam with one implementation.** The scan provider reads
  transcripts on demand and is sub-second for the scoped queries the API pushes
  you toward. The `Retrieval` contract and its conformance suite are kept as
  the seam a second provider would have to pass; a provider exercised through
  only one implementation is not a seam, so the suite asserts the contract's
  edges — coverage, cursors, windows — rather than one provider's habits.
- **Runtimes are configuration.** Every shipped runtime is a TOML descriptor
  the binary embeds; a file in `~/.teleport/runtimes.d/` with a known id
  overrides it whole, a new id adds a runtime, and neither needs a rebuild. A
  descriptor also declares what a runtime can do — whether it is scannable,
  multiplexes sessions in one process, sends heartbeats, owns a pane, how it
  is woken and how it should reply — so the daemon and the CLI branch on
  declared capabilities, never on a runtime's name. A format the descriptor
  language cannot express drops to a Rust `Adapter` impl.
- **macOS reach has a hard constraint.** A bare LaunchAgent cannot hold the
  Automation grant that AppleScript injection needs, so iTerm2 injection works
  only from paths a person started. tmux is the only grant-free injection path,
  and `fld` stays grant-free by construction.

**Where the reasoning is.** Comments state design: what a thing is and why it
is shaped that way. The incident or measurement that produced a decision is in
the commit that made it, where `git log -L` finds it. The modules worth reading
before changing anything near them:

- `crates/tp-core/src/turn.rs` — the vocabulary every layer speaks
- `crates/tp-core/src/retrieval.rs` — the retrieval contract: coverage,
  cursors, windows
- `crates/tp-db/src/reach/` — presence, addressing and the mailbox, one file
  per decision
- `crates/tp-ingest/src/adapter/decl/` — the descriptor engine
- `crates/tp-search/src/scan.rs` — the provider
- `crates/tp-reach/src/{resolve,discover,wake}.rs` — who is live, and how a
  session is reached
- `install/runtimes.d/*.toml` — one file per runtime


## The crate layout

Crates, in dependency order — the DAG is one-directional and deliberate:

| crate | what it owns |
| --- | --- |
| `tp-core` | types, ids, the retrieval contract. No I/O. |
| `tp-db` | SQLite, migrations, queries, the reach repository |
| `tp-ingest` | per-runtime transcript adapters, redaction |
| `tp-search` | Retrieval: the on-demand scan over transcript files |
| `tp-reach` | mailboxes, resolving a session, waking one |
| `tp-net` | HTTPS federation, host probing, pairing, RFC 9421 |
| `tp-serve` | the servers `fld` binds: the peer HTTPS API and the local unix socket |
| `tp-app` | the operations both surfaces call — returns values, never prints |
| `fl` | the binary: CLI (`main.rs`) + MCP server (`mcp.rs`) + daemon (`fld`) |

`tp-app` is the one worth knowing about: `main.rs` renders prose for a person
and `mcp.rs` serialises JSON for a model, and both call one operation that
decides nothing about presentation. A rule that lives in only one of those two
files is a divergence waiting to be found.


## Invariants

The rules the code cites by name. Each is enforced somewhere concrete; the
pointer is where to look when a comment appeals to it.

- **Coverage is a correctness contract.** A truncated or degraded scan must
  say so, because "not scanned" reported as "not found" is the false negative
  the whole retrieval seam exists to prevent. (`tp-core/src/retrieval.rs`,
  `Coverage`; asserted by the conformance suite.)
- **The universal cursor is `ts`, unix milliseconds.** Every paging surface
  resumes from a timestamp, and every clock stored is milliseconds; a test
  greps the SQL for the seconds idiom. (`tp-core/src/retrieval.rs`,
  `TurnCursor`/`admit_turn`.)
- **A session id is `<machine>/<runtime>/<native>`.** The machine part is
  derived from this machine's public key, so an id is self-authenticating.
  Register and search must compose the same id from the same parts, or an
  address copied from one surface silently never resolves on another.
  (`tp-core/src/id.rs`, `SessionId`.)
- **A conversation has one address across compaction.** A runtime that
  rewrites its transcript on compaction changes the native segment id; the
  `conv-<uuid>` address does not, so a message addressed before compaction is
  delivered after it. This is the identity peers hold, and it maps to A2A's
  `context_id`. (`tp-reach/src/resolve.rs`, where the address is minted;
  `tp-core/src/address.rs`, `Addressability`; classified in
  `tp-db/src/reach/address.rs`.)
- **Only a fixed control string ever crosses a pane.** Message content stays
  in the database; a wake types a constant from an operator-controlled
  descriptor, with no part derived from the request. A hostile peer can at
  most make a session check its inbox. (`tp-reach/src/wake.rs`.)
- **The envelope is open.** A message's `kind` is a string with three names
  floonet gives meaning to — `ask`, `note`, `reply` — and room for any other;
  `content_type` says what the body is and `extensions` carries fields this
  version does not know. Modelled on CloudEvents so a sender outside floonet
  does not need a change here. (`tp-app/src/send.rs`, `Kind`; migration 0019.)
- **A LaunchAgent does no AppleScript.** Injection needs an Automation grant
  a background daemon can never hold, so `fld` is restricted to grant-free
  backends by construction, not by hoping. (`tp-reach/src/wake.rs`, `Caller`.)
- **Descriptors are data, never scripts.** A runtime or terminal is described
  in TOML the binary embeds; user files override by id, whole-file; malformed
  files warn and are skipped, never fatal. (`tp-ingest/src/adapter/decl/`,
  `tp-reach/src/terminal.rs`.)
- **The data directory is owner-only, and things depend on that.** `tp-db`
  holds `~/.teleport` at 0700 and the store at 0600 on every open. The local
  socket's mode is set after it is bound, and the directory is what keeps the
  socket unreachable in between; a process-global umask would have narrowed
  every other thread's file creation instead. (`tp-db/src/lib.rs`, `restrict`;
  `tp-serve/src/local.rs`, `bind`.)


## Names

| Surface | Name |
|---|---|
| Product, in docs and user-facing strings | `floonet` |
| Binaries, what the user types | `fl` / `fld` |
| Crates | `tp-*` (internal) |
| Product environment variables | `TP_` prefix — `TP_PORT`, `TP_DB`, `TP_LOG_LEVEL`, `TP_LOG_FORMAT`. Bare legacy names are read as a fallback only |
| Data directory | `~/.teleport/` |
| launchd components | `io.teleport.*` |
| MCP and native tools | `floo_*` |
| Skill | `floonet`, in each harness's skill directory |
| Slash command (Claude Code) | `/floonet:inbox` |

For a new surface: a tool takes `floo_`, an environment variable takes `TP_`
(never a bare generic name — an unrelated ancestor process exporting
`LOG_LEVEL` would steer this software), a string that names the product says
`floonet` and one that names the command says `fl`.

Five of these are **stable identifiers**, and unifying them is not a cleanup:

- `floo_*` is the agent-facing API. Configured agents on other machines hold
  these names; renaming is a fleet migration, and an alias layer leaves two
  names per tool.
- `~/.teleport` — renaming orphans every existing install's mailbox, pairings
  and identity key unless a migration ships with it.
- `io.teleport.*` — plists installed on every machine carry the label;
  renaming orphans every daemon's autostart.
- `floonet` as the skill name — it is the directory three harnesses scan for,
  and the plugin name Claude Code and codex registered.
- `/floonet:inbox` — registered with Claude Code as the plugin's command, and
  the name the Claude Code descriptor's control string tells a woken session
  to run.
