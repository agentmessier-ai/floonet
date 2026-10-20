# Contributing

Patches welcome. This file is short on ceremony and long on the two or three
conventions that are genuinely unusual here — those are the ones a
well-intentioned patch gets wrong, and the ones a reviewer will push back on.

## Getting it building

macOS is the only platform floonet currently runs on: it reads transcripts
written by Mac-hosted agent runtimes and reaches sessions through tmux and
iTerm2. Rust 1.88 or newer — the floor comes from transitive dependencies, not
from this code, and it is declared in `Cargo.toml` so a too-old toolchain says
so by name.

```bash
cargo build --release
cargo t                          # the conformance suite runs against BOTH retrieval backends
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

`cargo t` is an alias in `.cargo/config.toml` for `test --workspace
--all-targets`, and the whole suite is about 3 seconds. Plain `cargo test
--workspace` runs the same 397 tests in over two minutes, because roughly 124 of
them go to rustdoc scanning every doc comment in nine crates for ```` ```rust ```` examples
and finding none. `cargo t-all` is the slow one, for the release gate.

**If `cargo t` takes minutes, look at `target/debug/deps` before looking at
Rust.** macOS Gatekeeper assesses every newly built binary on its first exec,
and the assessment scales with how many entries sit in the binary's directory.
That directory was once found holding 1.2 million stale `.o` files; each fresh
test binary then waited 10–26 s to start, and a `cargo t` after a one-line
change took 703 s of which libtest accounted for 6.6. `[profile.dev]` now sets
`split-debuginfo = "packed"` so the objects are folded into a `.dSYM` and
deleted instead of left behind — the comment there has the measurements. If a
rebuild still stalls between `Running` lines with the test process at 0 % CPU,
count the directory (`ls -f target/debug/deps | wc -l`); above a few tens of
thousands, `cargo clean` and start over.

Steady state on an M-series Mac: 3 s with no change, 4 s after touching `fl`'s
`main.rs`, 8 s after adding a public item to `tp-core`, 10 s after
`cargo clean -p` of all nine workspace crates.

Install the local hooks once. They run a ~1s subset of the CI gates before each
commit — formatting, clippy, and a secret scan of the staged content:

```bash
git config core.hooksPath .githooks
```

The hook's own header explains the budget it is designed to: anything slower
than a couple of seconds gets bypassed with `--no-verify`, and a bypassed hook
trains the reflex that makes the next bypass the one that mattered. Slow checks
belong in CI.

## If you are reading this outside the source repository

This repository is a published projection of a private one: a single commit,
assembled from an explicit manifest. Two things live in the private half and
are deliberately not here, and both change how a patch reaches us.

**There is no pre-commit hook.** `.githooks/` calls a tool that is not public,
so the `core.hooksPath` line above does nothing here. Run the checks by hand —
they are the same ones CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo t-all
```

Use `~/.cargo/bin/cargo` if Homebrew's Rust comes earlier on your PATH.
`rust-toolchain.toml` pins the compiler and only rustup's shim honours it, so a
plain `cargo` can be several minor versions ahead of what the gates run — which
matters when the gate is `clippy -D warnings` and a newer clippy has new lints.

**Pull requests here get no checks at all.** The only workflow in this
repository builds release binaries from a tag. Everything that judges a change —
lint, tests, MSRV, dependency audit, SAST — runs on a self-hosted runner in the
private repository, so a PR opened here shows no checks rather than failing
ones. A maintainer runs them before merging. That is slower for you, and it is
the honest version: an empty check list means nothing ran, not that nothing was
wrong.

## What CI enforces

`cargo fmt --check`, `clippy -D warnings`, the full test suite with a **65%
coverage floor**, an MSRV build, `cargo-deny` over the dependency graph, a SAST
pass, and a gitleaks scan. Jobs run on a self-hosted runner in the private
repository, so they may queue behind each other rather than starting
immediately. In the published repository they do not run at all — see the
section above.

`clippy::pedantic` and `clippy::nursery` are *not* enabled and are not expected
to be clean. They are opt-in lint groups the Rust project does not intend
anyone to zero out; treating their output as a backlog is a misreading.

## The conventions that actually matter

**A type must not merge outcomes a caller would act on differently.** This is
the house rule with the most history behind it. `Addressability` keeps
"registered", "a dormant conversation", "dormant" and "unknown" apart because a
caller does something different for each, and collapsing them once caused
floonet to report a message as delivered when nothing could ever read it.
`EmptyScan` separates "authoritatively empty" from "the scan failed" for the
same reason. Merging two variants needs an argument; splitting one does not.

The same rule applies to a schema column, not just a Rust type. `message.
read_at` used to mean both "shown to a drain" and "the recipient finished
acting on it" — one timestamp standing in for two different facts a caller
needed to tell apart. An agent interrupted between the two (compaction, a
crash) left a message read and permanently unrecoverable, indistinguishable
from one that had genuinely been handled. `acked_at` (migration 0009) split
it into a Kafka-style poll/commit pair — the module header in
`crates/tp-reach/src/mailbox.rs` holds the schema and the reasoning;
`fl inbox --pending` is the recovery view that split makes possible.

If you find yourself adding a `bool` where a third state is imaginable, add the
enum instead — and if it's a column rather than a Rust type, ask the same
question of it.

**A failure must never be rendered as a fact.** The recurring bug in this
codebase is absence of evidence read as evidence of absence: a scan that failed
reported as an empty machine, a default search window reported as an exhaustive
search, a database error reported as `known: false`. If your code can fail to
find out, say that it failed to find out.

**Observe a test failing for the right reason.** A new test that has never been
red proves nothing. Break the rule it covers, watch exactly that test go red,
put the rule back. If a change is meant to fix a bug, the test should fail
before the fix and pass after — and the commit message should say you checked.
Several tests here were written, passed immediately, and were later found to be
asserting nothing at all.

**Comments state design, not history.** A comment says what a thing is and
why it is shaped that way: the constraint, the invariant, the alternative that
was rejected and the property that rules it out. It does not carry the incident
that motivated it — no dates, commit hashes, measurements or "used to". That
record lives in the commit message, where `git log -L` can find it. A module
doc is a few lines; a function doc is a sentence or two; an inline comment is
one. If you change behaviour a comment explains, change the reasoning to match.

**Behaviour lives in `tp-app`, rendering lives in the surfaces.** `main.rs`
prints prose for a person; `mcp.rs` serialises JSON for a model; both call one
operation in `tp-app` that returns a value and decides nothing about
presentation. A patch that puts a rule in one surface will be asked to move it,
because every divergence between those two files has so far been a bug.

**The crate DAG is one-directional.** `tp-core` → `{tp-db, tp-ingest}` →
`{tp-search, tp-reach}` → `tp-net` → `tp-app` → `fl`. A new
dependency that points the other way is a design change, not a convenience.

## Tests

Unit tests live beside the code. Integration tests that need a real binary live
in `crates/tp/tests/`.

**A new integration test file goes in `tests/it/` and gets a `mod` line in
`tests/it/main.rs`.** Not `tests/whatever.rs`, which is the layout cargo
documents and the one this repository deliberately does not use: every top-level
file under `tests/` is its own crate and its own full link, so twenty test files
means twenty copies of the whole dependency graph linked separately. Touching
one line in `crates/tp/src/main.rs` used to cost 64 seconds; as one binary per
crate it costs 7.8. The thing given up is `cargo test --test <file>`, since
there is now one target called `it` — filter by name instead, `cargo t
reply_hint`, which is what most people were doing anyway.

The conformance suite in `crates/tp-search/tests/` is
the important one: every retrieval behaviour is asserted against **both** the
on-demand scan backend and the SQLite index, because the two answering
differently is the failure mode that matters most and is invisible from either
side alone.

There is also a differential check that proves the shipped runtime configs
still agree with the built-in adapters, over every session file on the machine
it runs on:

```bash
cargo run --release -p tp-ingest --example differential
```

## Commits

Conventional-commit prefixes (`fix:`, `feat:`, `refactor:`, `docs:`, `ci:`,
`security:`), with the crate or area in parentheses where it helps —
`fix(reach):`, `refactor(app):`.

The body is where the value is. Say what was wrong, how you know, and what you
verified — including what you did *not* verify. Commit messages here are the
project's incident record and are read far more often than they are written.

## Pull requests

One concern per PR. If a refactor and a fix are in the same branch, the fix is
the one nobody can review.

Say in the description what you ran and what it printed. "Tests pass" is worth
less than the numbers, and a claim that something was verified against the real
binary should show the output.

## Reporting a security issue

Do not open an issue — see [`SECURITY.md`](SECURITY.md).
