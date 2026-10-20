#![allow(clippy::print_stdout)]
//! Exempt from the workspace `print_stdout` lint for one line: `--version`
//! answers on stdout because a caller pipes it. `print_stderr` is not exempt:
//! every diagnostic goes through `tp_core::logging`, so it carries a timestamp,
//! which a daemon log needs most.

//! `fld` — the resident daemon. Two jobs, one process: serve the peer HTTP API
//! (`tp-net::server`) and keep `live_session` honest via the discovery scan.
//!
//! TCC-free by design: it binds a socket, reads `~/.claude`, runs `ps`/`lsof`
//! and writes SQLite — all of which a bare LaunchAgent can do without prompts.
//! Injection needs Automation and lives in `tp`, which a human runs from a GUI
//! session that can hold the grant (see `tp_reach::discover::scan_all`).
//!
//! A separate binary rather than `fl serve` because it is a different kind of
//! program: `tp` is stateless and exits; `fld` is a supervised long-lived
//! service whose lifecycle launchd owns.

use anyhow::{Context, Result};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<()> {
    // Answered before the bind: asking a supervised daemon its version must
    // work while a copy of it is already running, which is when it gets asked.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("fld {}", tp_core::VERSION_LINE);
        return Ok(());
    }

    // Before anything that can log: `tp` and `fld` share a stderr convention
    // and, under launchd, a file, so the service field is what tells them apart.
    tp_core::exit_quietly_on_broken_pipe();
    tp_core::logging::set_service("tpd");
    // launchd's stderr file, bounded before `to_file`: a daemon that cannot
    // start writes its error here and is restarted forever, so the file grows
    // exactly while nothing is running to rotate it.
    tp_core::logging::bound_foreign_log(
        &tp_db::teleport_dir().join("tpd.err.log"),
        tp_core::logging::MAX_BYTES,
    );
    // The daemon writes its own log so it can rotate it. launchd's stderr file
    // stays as the capture for panics and anything before this line.
    let log_path = tp_db::teleport_dir().join("tpd.log");
    if let Err(e) = tp_core::logging::to_file(&log_path) {
        // Stderr is still the fallback inside `emit`, so this degrades to
        // launchd's capture rather than to silence.
        tp_core::log_warn!(
            "cannot write {}: {e:#} — logging to stderr",
            log_path.display()
        );
    }

    let identity = tp_net::Identity::load_or_create(&tp_net::identity::default_key_path())
        .context("load device identity")?;
    let name = tp_net::identity::hostname();

    tp_core::log_info!(
        "tpd {} — {} ({})",
        tp_core::VERSION_LINE,
        name,
        identity.device_id
    );

    // Provider::Scan: a scan reads the transcripts themselves, so a peer's
    // answer never depends on anything having been copied into this database.
    let app = tp_app::App::open(
        &tp_db::default_db_path(),
        &tp_net::identity::default_key_path(),
    )
    .context("open app")?;
    app.ensure_self_machine()?;

    // Read once, before `app` moves into the server state, so every line below
    // agrees on one answer.
    let listen = app.peer_listen().context("read peer-listen setting")?;
    let port = listen.port;

    let state = tp_serve::state_from(app);

    // The local adapter, over the same `App` the peer server holds. A separate
    // listener because the audiences differ: a trusted peer over TLS with a
    // signature, and this machine's panel over a mode-0600 socket.
    //
    // A failure here does not stop the daemon: the two are independent failure
    // domains, and giving up would also cost the discovery scan, delivery and
    // every peer read. The error is logged with the path so it is findable.
    let sock = tp_db::daemon_socket_path();
    match tp_serve::local::bind(&sock) {
        Ok(listener) => {
            tp_core::log_info!("local api on {}", sock.display());
            let app_for_local = state.app.clone();
            tokio::spawn(tp_serve::local::serve(listener, app_for_local));
        }
        Err(e) => tp_core::log_warn!(
            "local api unavailable ({}): {e} — the panel will fall back to reading the database",
            sock.display()
        ),
    }

    // 0.0.0.0: peers are by definition not on loopback. Every non-loopback
    // request still has to carry a valid signature from a trusted peer.
    if listen.enabled {
        let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
        let bound = tp_serve::serve(state, addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        tp_core::log_info!("serving on {bound}");
    } else {
        // Said, not silent: someone who expects to be reachable must learn it
        // from the log rather than from a pairing that never completes.
        tp_core::log_info!(
            "peer port CLOSED — this machine is not reachable from another. Turn \
             it on in the panel, or with `fl listen on`. Everything local — the \
             mailbox, waking a session, search — is unaffected."
        );
    }
    // Publish what is running — after the bind attempt, so a second instance
    // that loses the port race exits without having claimed `daemon_status`;
    // a dead pid recorded there answers "is the daemon stale?" with a process
    // that does not exist. With the peer port closed there is no race, and the
    // row is still written: the daemon is running the scan and the mailbox,
    // and not listening is a configuration choice, not a fault.
    //
    // A fresh handle: `app` moved into `state_from` above.
    tp_db::Db::open(&tp_db::default_db_path())?
        .record_daemon_start(tp_core::VERSION_LINE, std::process::id())?;
    // A stale override makes a freshly installed daemon run old parsing rules
    // while reporting a new version, so it is named at startup.
    for o in tp_ingest::adapter::descriptor_overrides() {
        if !o.identical {
            tp_core::log_warn!(
                "descriptor override {} differs from the embedded {} — customized or stale; `fl version` explains",
                o.path.display(),
                o.id
            );
        }
    }

    // No transcript watcher. A transcript is read where it lies, by the scan
    // provider, on the query that asks for it: no second writer racing this
    // daemon, no checkpoint to go stale, no copy of the corpus. The cost is
    // history a runtime has since deleted. `fl ingest` still writes, for
    // runtimes that keep no transcript of their own.

    // Its own `App` rather than a share of the server's: this thread writes on
    // every cycle, and behind the server's mutex it would park ping and pairing
    // behind a process scan. A separate `App` is a separate WAL connection.
    let scan_app = tp_app::App::open(
        &tp_db::default_db_path(),
        &tp_net::identity::default_key_path(),
    )
    .context("open app for discovery scan")?;
    std::thread::spawn(move || run_discovery_scan(scan_app));

    // Serve until stopped, and stop by RETURNING rather than by dying.
    //
    // The plist asks for a restart only on an unclean exit, so a clean return
    // is what makes `launchctl bootout` final instead of the first half of a
    // respawn loop. A process terminated by a signal is not a clean exit.
    //
    // Both signals, because the two ways this daemon is stopped send different
    // ones: a developer in a terminal sends SIGINT, launchd sends SIGTERM.
    // SIGTERM at its default disposition does end the process, which is why
    // this looked like it worked.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    let signal = tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = term.recv() => "SIGTERM",
    };
    tp_core::log_info!("shutting down ({signal})");
    Ok(())
}

/// Active-scan discovery: authoritative for `live_session` liveness every
/// `SCAN_INTERVAL_SECS` — adds what hook registration missed, prunes what it
/// never cleaned up. A failed cycle is logged and the loop continues; one
/// transient `lsof`/`osascript` hiccup must not take the daemon down.
fn run_discovery_scan(app: tp_app::App) {
    // Read once at startup: descriptors change when a user edits
    // ~/.teleport/runtimes.d and restarts, the same lifecycle as the roots.
    let signatures: Vec<tp_reach::discover::ProcessSignature> =
        tp_ingest::adapter::process_signatures()
            .into_iter()
            .map(
                |(runtime_id, pattern)| tp_reach::discover::ProcessSignature {
                    runtime_id,
                    pattern,
                },
            )
            .collect();
    if signatures.is_empty() {
        tp_core::log_warn!("no harness declares a process signature — scan discovery is inert");
    }

    // Sessions already read and found to be real conversations. The sweep below
    // runs every cycle, and this is what makes that affordable: a `locate` plus
    // a first-line read is paid once per session, not once per session per
    // interval. Deliberately not persisted — a fresh daemon re-reads everything
    // once, which costs one cycle and removes any chance of a stale verdict
    // outliving the file it was formed from.
    //
    // Only definite verdicts land here. "Cannot tell" is the state every
    // session passes through before its transcript's first line is written, and
    // caching it would make the sweep permanently blind to the newest rows.
    let mut judged: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Consecutive cycles that found nothing. Reset on any non-empty scan.
    let mut empty_cycles = 0u32;

    loop {
        // Scan first, then sleep, so a restart does not leave `live_session`
        // stale for a full interval before the first reconcile.
        // Declared sessions expire on their own heartbeat, not on the scan —
        // two halves of one job sharing a cycle. A failure in either half
        // must not skip the other.
        match app.sweep_declared() {
            Ok((marked, evicted)) if marked > 0 || evicted > 0 => {
                tp_core::log_info!("presence sweep marked {marked} stale, evicted {evicted}");
            }
            Ok(_) => {}
            Err(e) => tp_core::log_warn!("presence sweep failed (will retry): {e:#}"),
        }

        // Claude Code spawns headless `-p` sub-conversations inside a live pane;
        // each registers like a conversation, and a hook row is removed only by
        // the SessionEnd of the process that made it. A sub-call KILLED rather
        // than exited never fires one — verified by experiment: three spawned
        // and signalled, three rows left behind, none unregistered — and the
        // row then carries the pane's pid, so reconcile refreshes it for as long
        // as the pane lives and staleness never evicts it.
        //
        // Per cycle rather than once at startup, which is what this used to be.
        // The cost that bought "startup only" was a `locate` plus a file read
        // per hook row per interval, forever; `judged` pays it once per session
        // instead, which leaves only rows the sweep has never seen — a handful
        // a day, the figure the original reasoning assumed. What that buys is
        // the difference between "clean after a restart" and "clean within a
        // minute", and one interrupted pipeline run is what showed the gap:
        // 54 rows on one pane, a restart away from going.
        match app.sweep_non_conversations(&mut judged) {
            Ok(0) => {}
            Ok(n) => tp_core::log_info!(
                "swept {n} registration(s) that were not conversations \
                 (headless sub-conversations whose pane is still running)"
            ),
            // Never fatal: an unreadable transcript must not cost the cycle its
            // other half, nor the daemon its life.
            Err(e) => tp_core::log_warn!("non-conversation sweep failed (will retry): {e:#}"),
        }

        let found = tp_reach::scan_all(&signatures);
        // An empty scan is only believed on the second consecutive cycle: the
        // scan's inputs (`ps`, tmux, osascript) all degrade silently to empty,
        // so one empty result cannot be told from "could not look", and acting
        // on it deletes every scan-tracked session. One cycle of delay is free;
        // the sessions stay addressable throughout. Same grace-then-act shape
        // as `sweep_declared`.
        if found.is_empty() {
            empty_cycles += 1;
        } else {
            empty_cycles = 0;
        }
        let empty = if empty_cycles >= 2 {
            tp_reach::discover::EmptyScan::Authoritative
        } else {
            if empty_cycles == 1 {
                tp_core::log_info!(
                    "scan found nothing — not pruning yet (a failed `ps`/tmux/osascript looks \
                     identical to an idle machine; confirming on the next cycle)"
                );
            }
            tp_reach::discover::EmptyScan::Unverified
        };
        if let Err(e) = app.reconcile_scan(&found, empty) {
            tp_core::log_warn!("discovery scan cycle failed (will retry): {e:#}");
        }
        std::thread::sleep(std::time::Duration::from_secs(tp_reach::SCAN_INTERVAL_SECS));
    }
}
