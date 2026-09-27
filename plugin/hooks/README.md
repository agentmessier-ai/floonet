# `hooks.json`

Session registration, declared by the plugin rather than written into the
user's `settings.json` by `install.sh`.

## Why this prose is here and not in the file

It used to be, under a `_comment` key. Claude Code ignores unknown fields;
**codex does not**, and it is the second consumer of this exact file:

```
⚠ failed to parse plugin hooks config
  ~/.codex/plugins/cache/floonet/floonet/0.2.1/hooks/hooks.json:
  unknown field `_comment`, expected `description` or `hooks` at line 2 column 12
```

The whole file is rejected on that one key, so **no hook is registered at all**
— codex sessions never call `fl register`, never get a `live_session` row, and
`fl ask` to one lands in the mailbox and wakes nothing. Exactly the failure the
hooks exist to prevent, caused by the comment explaining them.

The irony is on the record: the old `_comment` correctly said "TWO consumers
read this one file: Claude Code, and codex". Knowing a second parser reads a
file is not the same as writing to what it accepts.

`hooks.json` now carries only `description` and `hooks` — the two fields codex
names — and everything else lives here.

## What the hooks do

These two are what makes a session REACHABLE. `fl register --from-hook` reads
the event JSON on stdin, takes `session_id` and `cwd` from it, walks up the
process tree for the hosting pid and tty, and writes the `live_session` row that
`fl ask` resolves against. Without them `resolve()` finds no row and returns
`Target::NotLive`.

`$HOME` expands here — verified against a real Claude Code build rather than
assumed, along with the fact that a plugin hook receives the identical event
JSON a `settings.json` hook does. The absolute path is deliberate:
`~/.local/bin` is not guaranteed to be on `PATH` for a hook process, and
`install.sh`'s own closing line still says so.

## Why `UserPromptSubmit` re-registers

`SessionStart` fires ONCE. A Claude Code or codex session has no heartbeat, so
that single row is the only thing making it reachable for its entire life — and
anything that empties floonet's database takes it away permanently. Uninstall,
reinstall, a manual delete: every session alive at that moment keeps running,
keeps being found by the process scan, keeps appearing in `fl live`, and can
never be read again, because floonet no longer knows its real session id.

Measured, on the machine this was written on: a session started at 09:04
registered normally; the database was recreated at 12:24 during a reinstall;
the session ran on for another twelve hours as `scan-pid-10730`. Asked what that
session was doing, another agent called `floo_turns` on that address, got
nothing, picked a different id that looked plausible, and reported a different
folder's conversation under this one's name.

This was carried as a known issue for weeks — "hook registration rows
disappear, cause unknown". The cause is not a lost row. It is a row that was
correctly written to a database that no longer exists, by a hook that will never
fire again.

So registration renews on every prompt. `fl register` is an upsert keyed on
session id, so the steady-state cost is one spawn and one `UPDATE` per turn, and
the failure it repairs is otherwise unrecoverable without restarting the session.

Output is discarded and the command always exits 0 — the ONLY hook here that
does. A `SessionStart` failure is worth seeing, because the session is
unreachable until someone acts on it. A per-prompt renewal failing is not worth
interrupting a turn for: the scan still finds the process, the session is still
messageable, and the next prompt tries again a few seconds later. What is not
acceptable is an error banner on every single prompt because one harness shaped
its event payload differently than the other.

## Running twice is safe

Plugin hooks run ALONGSIDE user hooks, not instead of them, so a machine still
carrying the older `setup-hooks.py` entries registers twice per session.
Measured rather than assumed: against an existing index that is fine, 3 of 3
concurrent rounds exit 0, because `register` is an upsert keyed on session id
and `Db::open` sets `busy_timeout` to 5s.

The one case that DOES lose is two processes creating and migrating a brand-new
index at the same moment — the very first session after a fresh install, where
one side can see "database is locked". It self-corrects on the next session, and
the mailbox is unaffected.

## No `--runtime` flag, deliberately

Two harnesses read this one file, so either name hardcoded would mislabel the
other's sessions. `fl register` works the runtime out itself —
`runtime_for_registration` walks the process ancestry against every descriptor's
`process_match`. The day this file said nothing, the old default silently
composed codex sessions as `<machine>/claude_code/<codex id>`, an address
nothing could deliver to.

## Verifying codex actually picked them up

codex trust-gates hooks per file path and hash (`[hooks.state]` in its
`config.toml`), so a changed `hooks.json` re-prompts. Two ways to check:

- a `[hooks.state]` entry naming the plugin cache path
- `fl live` showing `hook` rather than `scan` beside a codex session

The second is the one that matters — it is the difference between floonet
having found the session and the session having announced itself.
