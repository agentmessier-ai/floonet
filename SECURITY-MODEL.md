# Security model

The full model behind the [summary the README carries](README.md#security-model).
Process — reporting, scope, supported versions — lives in [SECURITY.md](SECURITY.md).

The reach path is built on one hard rule:

> **Only a fixed control string is ever typed into another session's pane.**
> Real content stays in the mailbox, where the receiving agent reads it through
> a tool call it can reason about and refuse.

Agents on this machine may be running with `--dangerously-skip-permissions`. If
message *content* were typed into a pane, anything that could reach the pane
would have arbitrary execution with no gate. Because only the control string
crosses, the pane is not an injection surface at all: the worst a poke can do is
make a session **check its inbox**.

**One designed-in exemption: `fl type`.** `fl type <tty> <text>` (library:
`tp_reach::wake::type_raw`) types *arbitrary* text into a terminal pane. It
exists for targets that have no inbox of their own to dereference through —
a different agent CLI that simply reads keyboard input — and it carries none
of the guarantee above: whatever is typed lands in that terminal's input
exactly as if a person had typed it. It is contained three ways: it is a CLI
command only, **never an MCP tool**, so no agent can invoke it on its own
judgement; no reach path uses it (`fl ask` / `floo_ask` only ever send the
fixed control string); and its help text says it is not the safe path. Every
other byte that crosses any pane in this system is that one control string.

**floonet stores no conversations.** `~/.teleport/` holds the mailbox, which
sessions are live, the machines you paired with, and the identity key. A search
reads the runtime's own transcript on the query that asks for it. Earlier
versions kept a second copy in SQLite; that store, and everything that wrote it,
is gone. The practical consequence for this document: floonet's retention is not
floonet's to state — it is each runtime's.

**The trust boundary is the mailbox, not the prompt.** Today its only writers
are local: `fl ask`, `fl note` and `floo_ask` — that is, anything running
as you — and the database itself is writable by the same set. There is
**no network route to the mailbox at all**: the daemon's HTTP surface is
peer-facing only, with no loopback exemption, and cross-machine reach is
designed, not wired. A peer you approved by hand can put nothing in it yet;
when reach ships, that approval — fingerprints compared out of band — is
what will gate it. Inside the boundary a message is a *task*: the receiving
agent does the work and replies, applying the same judgement about risk it
would to a request typed by its operator. It is not given a separate, stricter rule for
messages, because a prompt-level rule was never an enforceable one — Claude Code
and Pi can prompt it, not enforce it, and leaning on it would have been
security theatre in place of the boundary that actually holds.

The practical consequence, stated plainly (once cross-machine reach ships —
today it is designed, not wired): **pairing a machine grants it the ability
to make your agents do work.** Pair only machines you'd hand a keyboard.
If you want a stricter posture than that, the place to put it is the receiving
agent's own permission gate, which floonet does not bypass.

- **Nothing listens by default** — a fresh install opens no port. floonet is
  local: it reads transcript files, writes one SQLite file, and types into
  terminals on this machine. The peer listener is the single thing that makes an
  install reachable from outside, so it is off until `fl listen on` (or the
  panel's switch) turns it on, and `fl listen` says which it is. Pairing does not
  need it — `fl pair` is local and human-approved — so a machine can be paired
  first and start listening later, or never. This is deliberate rather than
  conservative defaults for their own sake: "security is the operator's to add"
  cannot cover a default, because a default is not something the operator added.
- **Identity** — ed25519 keypair generated at install, private key `0600` at
  `~/.teleport/key`. The device fingerprint is what you compare out of band.
  Both the key and the database are `0600` — and the database is
  re-restricted on **every** open (`tp_db::Db::open`), so installs made while
  the file was still at the umask default healed in place rather than being
  recreated. The boundary today is the local account; the file mode is the
  second door behind it.
- **Pairing** — Syncthing-style: explicit request, then a human `approve` on
  *both* sides after comparing fingerprints. Trust is never automatic — and
  once granted it is permanent, unscoped, and unlogged: no expiry, no
  per-call scope, no audit table. `fl pair revoke` is the only undo.
- **Peer requests** — signed per RFC 9421 over TLS. The TLS is unpinned by
  design: its job is to stop a passive listener, so an active on-path attacker
  can relay the signed request and read the response plaintext — authenticity
  is the ed25519 signature, not the transport. Reads are timestamp-bound.
  There is **no network write route at all**: pairing approval happens only via
  `fl pair approve`, which writes to the database directly. The HTTP endpoint
  that used to accept it was removed — it had no legitimate caller, and any
  local process could use it to make a remote machine permanently trusted.
- **Redaction** — every result passes one mandatory scrub funnel: `Hit` is only
  constructible through it, so no backend can skip it. The *patterns* are
  best-effort (AWS keys, `sk-ant-*`, `ghp_*`, private keys, bearer tokens, …) —
  a secret matching none of them passes through. The structural guarantee is
  that scrubbing is never skipped, not that every secret shape is known.
- **What a trusted peer can read** — everything the local search can, which is
  every transcript still on this machine: floonet keeps no copy of a
  conversation, so the reachable history is whatever each runtime has not yet
  deleted (Claude Code prunes after about a month). `thinking` is included when
  the request asks for it, plus session titles and working-directory paths via
  `/v1/sessions`. Results are scrubbed excerpts, not whole turns, but there is
  no cap on how many a peer may page through and no owner-side scope — no
  folder, period, or field is withheld from an approved peer. Trust is the only
  boundary, which is why granting it is a CLI step and not a tool.
- **Reach guards** — ≤1 wake per target per 10 s; a `MAX_DELIVER` cap parks an
  undrained inbox instead of waking a session forever.

