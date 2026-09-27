#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt from the workspace print lints: this module's output is the product.
//! See [workspace.lints.clippy] in the root Cargo.toml for why the lint exists
//! everywhere else.

//! Network and identity commands: id, live, peers, discover, version, pairing.

use crate::{app, db_path, hostname, identity};
use anyhow::Result;

pub(crate) fn run_id() -> Result<()> {
    let id = identity()?;
    println!("device id : {}", id.device_id);
    println!("name      : {}", hostname());
    println!("port      : {}", tp_net::serve_port());
    println!("\nCompare the device id out of band before approving on either side.");
    Ok(())
}

pub(crate) fn run_live() -> Result<()> {
    let rows = app()?.live()?;

    if rows.is_empty() {
        println!("no live sessions known — either none are running, or `fld` hasn't completed its first scan cycle yet (every {}s)", tp_reach::SCAN_INTERVAL_SECS);
        return Ok(());
    }
    for row in &rows {
        // 'hook' rows carry a real session_id from the runtime; 'scan' rows are
        // inferred and may be a synthetic `scan-pid-N` placeholder.
        println!(
            "{:<6} pid {:<8} tty {:<12} last seen {}",
            row.row.source,
            row.row.pid,
            row.row.tty.as_deref().unwrap_or("(none)"),
            crate::fmt_ts(Some(row.row.last_seen_at))
        );
        // Publish the conversation address when there is one: this listing is
        // where senders copy from, and a segment id stops being deliverable at
        // the target's next compaction. The segment id is still printed because
        // `fl turns` and stored messages are keyed by it.
        if row.address_is_stable() {
            println!(
                "       {}   (stable address — survives compaction)",
                row.address
            );
            println!("       {}   (current segment)", row.row.session_id);
        } else {
            println!("       {}", row.address);
        }
        // A synthetic address is deliverable and not readable, and without this
        // line it looks identical to a real one — an agent that reads "no turns"
        // from it goes looking for a plausible other id. The remedy is named
        // here because only the operator can apply it: a session registers once
        // at SessionStart with no heartbeat, so a recreated database loses it
        // until the session restarts.
        if row.address_is_synthetic() {
            println!(
                "       ^ scan-discovered, NOT registered: floonet does not know this session's \
                 real id, so it can be MESSAGED but not READ (`fl turns` finds nothing). It \
                 registers once at startup and never again — if floonet's database was recreated \
                 while it was running, that row is gone for good. Restart the session to restore it"
            );
        }
        if let Some(cwd) = &row.row.cwd {
            println!("       {cwd}");
        }
    }
    println!("\n({} live session(s))", rows.len());
    Ok(())
}

pub(crate) fn run_peers() -> Result<()> {
    let peers = app()?.peers()?;
    if peers.is_empty() {
        println!(
            "no peers yet — `fl discover` to find them, `fl pair request <host:port>` to introduce"
        );
        return Ok(());
    }
    for p in &peers {
        println!(
            "{:<9} {:<24} {:<22} last seen {}",
            p.trust,
            p.name,
            p.addr.as_deref().unwrap_or("(no address)"),
            crate::fmt_ts(p.last_seen_at)
        );
        println!("          {}", p.id);
    }
    Ok(())
}

pub(crate) fn run_discover(host: &str) -> Result<()> {
    let app = app()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let found = rt.block_on(app.discover(host))?;

    for p in &found.peers {
        println!(
            "{:<22} {:<24} {}",
            p.addr,
            p.name,
            if p.known { "known" } else { "new" }
        );
        println!("          {}", p.device_id);
    }
    if found.peers.is_empty() {
        if found.answered > 0 {
            println!("{host} is this machine — nothing to pair with");
        } else {
            println!("no floonet daemon answered on {host}");
            println!(
                "(tried ports {}-{}; give an explicit host:port if it listens elsewhere)",
                tp_net::DEFAULT_PORT,
                tp_net::DEFAULT_PORT + tp_net::PROBE_PORTS - 1
            );
        }
    }
    Ok(())
}

/// Swap back to the binaries the last install replaced; `install.sh` keeps one
/// generation beside the live ones. Both or neither: `fl` and `fld` share a
/// database schema, and a daemon running a migration the CLI does not know is
/// worse than the failure being escaped.
pub(crate) fn run_rollback() -> Result<()> {
    // `"/"` on a missing HOME, like every other HOME read in the workspace: an
    // empty default would make the path relative to the cwd.
    let bin = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
        .join(".local")
        .join("bin");
    let pairs: Vec<_> = ["fl", "fld"]
        .iter()
        .map(|b| (bin.join(b), bin.join(format!(".{b}.prev"))))
        .collect();

    for (_live, prev) in &pairs {
        if !prev.exists() {
            anyhow::bail!(
                "no previous build to roll back to ({} is missing).\n\
                 install.sh keeps one generation, so this is what a FIRST install looks like —\n\
                 there is nothing behind it yet. To go back further, reinstall a release:\n\
                 \thttps://github.com/agentmessier-ai/floonet/releases",
                prev.display()
            );
        }
    }

    // Swap rather than overwrite, so a rollback is itself reversible: running
    // this twice returns to the starting point.
    for (live, prev) in &pairs {
        let scratch = live.with_extension("rollback-tmp");
        std::fs::rename(live, &scratch)?;
        std::fs::rename(prev, live)?;
        std::fs::rename(&scratch, prev)?;
    }

    // Not `VERSION_LINE`: that names the build being rolled away from. Ask the
    // binary now in place.
    let now = std::process::Command::new(&pairs[0].0)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "(could not read the restored binary's version)".to_string());
    println!("rolled back to {now}");
    println!("  (the build that was live is now the one `fl rollback` would restore)");

    // A LaunchAgent keeps running whatever it started with, which is the
    // version being rolled away from. `io.teleport.tpd` is a stable identifier
    // named by installed plists; do not rename it.
    println!();
    println!("The daemon is still running the old code. Restart it:");
    println!("  launchctl kickstart -k gui/$(id -u)/io.teleport.tpd");
    Ok(())
}

/// Both builds, and whether they agree. The binary on disk is not what serves
/// peer requests — a LaunchAgent keeps running whatever it started with — so
/// "did my upgrade take" is only answerable by comparing the two.
pub(crate) fn run_version() -> Result<()> {
    println!("fl   {}", tp_core::VERSION_LINE);

    // `?`, not `.ok()`: opening the database runs the migrations, and a
    // command whose job is "did my upgrade take" must not print success over a
    // failed schema upgrade. The `.ok()` on `daemon_status` is tolerant on
    // purpose: a missing row means the daemon has not started since the
    // feature shipped, which is what the message below says.
    let app = app()?;
    let daemon = app.daemon_status().ok().flatten();

    let Some(d) = daemon else {
        println!("fld  not recorded — the daemon has not started since this feature shipped");
        print_descriptor_overrides(&app);
        print_backup_age();
        return Ok(());
    };
    println!(
        "fld  {}  · pid {} · up {}",
        d.version,
        d.pid,
        fmt_uptime((tp_core::now_ms() - d.started_at) / 1000)
    );

    match tp_core::compare_builds(&d.version, tp_core::VERSION_LINE) {
        tp_core::BuildMatch::Different => {
            println!(
                "\nThe daemon is running different code than this binary.\n\
                 Restart it:  launchctl kickstart -k gui/$(id -u)/io.teleport.tpd"
            );
        }
        // Not silence: saying nothing would read as "they match", which a
        // dirty tree cannot support.
        tp_core::BuildMatch::Unknown => {
            println!("\n(built from an uncommitted tree — the two cannot be compared)");
        }
        tp_core::BuildMatch::Same => {}
    }
    print_descriptor_overrides(&app);
    print_backup_age();
    Ok(())
}

/// How long since the last backup, and how much has no other copy. Here as
/// well as in `fl verify` because `fl version` is what a person runs without
/// being worried, which is when this is worth learning. Silent when nothing is
/// irreplaceable: transcripts still on disk can be re-read.
fn print_backup_age() {
    let Ok(app) = app() else {
        return;
    };
    let Ok((sessions, turns)) = app.irreplaceable() else {
        return;
    };
    if sessions == 0 {
        return;
    }

    match app.backup_status() {
        Ok(Some(b)) => {
            let days = (tp_core::now_ms() - b.taken_at) / 86_400_000;
            let when = match days {
                0 => "today".to_string(),
                1 => "yesterday".to_string(),
                d => format!("{d} days ago"),
            };
            // The turn delta is what makes the age actionable: the same age is
            // fine on an idle machine and alarming on a busy one.
            let now: i64 = app.turn_count().unwrap_or(b.turn_count);
            let drift = now - b.turn_count;
            println!(
                "\nbackup  {when} → {} ({} turn(s) since)",
                b.dest,
                if drift > 0 {
                    drift.to_string()
                } else {
                    "0".into()
                }
            );
        }
        // Never backed up is not "0 days ago"; it is the state a fresh install
        // stays in until someone acts.
        Ok(None) => {
            println!(
                "\nbackup  NEVER — {turns} turn(s) across {sessions} session(s) exist only here.\n\
                 \tfl backup ~/floonet-backup.db"
            );
        }
        Err(_) => {}
    }
}

/// Files in `~/.teleport/runtimes.d/` shadowing a built-in runtime. The binary
/// carries the shipped descriptors, so an override is either a customization
/// (it wins) or a stale copy from an older install. Content cannot tell the
/// two apart; naming the file and the difference is the whole job.
pub(crate) fn print_descriptor_overrides(app: &tp_app::App) {
    let overrides = app.descriptor_overrides();
    if overrides.is_empty() {
        return;
    }
    println!();
    for o in overrides {
        if o.identical {
            println!(
                "descriptor override {} is byte-identical to this build's embedded {} —                  redundant today, stale the next time the shipped descriptor changes; safe to delete",
                o.path.display(),
                o.id
            );
        } else {
            println!(
                "descriptor override {} DIFFERS from this build's embedded {} and is what actually runs.",
                o.path.display(),
                o.id
            );
            println!("  If you customized it: working as intended.");
            println!(
                "  If you did not: it is a stale copy from an older install — delete it, the binary carries the current one."
            );
        }
    }
}

pub(crate) fn fmt_uptime(secs: i64) -> String {
    match secs {
        s if s < 0 => "?".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// `192.0.2.42` → `192.0.2.42:47400`; anything that already names a port is
/// left alone. `fl discover` accepts a bare host, and `fl pair request` is
/// typed by copying the host out of it, so a bare host here must mean the same
/// port rather than the URL default of 443.
fn with_default_port(addr: &str) -> String {
    use std::net::{IpAddr, SocketAddr};
    if addr.parse::<SocketAddr>().is_ok() {
        return addr.to_string();
    }
    // A bare IPv6 literal is full of colons, so "contains a colon" cannot mean
    // "has a port" until this case is out of the way.
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return match ip {
            IpAddr::V6(v6) => format!("[{v6}]:{}", tp_net::DEFAULT_PORT),
            IpAddr::V4(v4) => format!("{v4}:{}", tp_net::DEFAULT_PORT),
        };
    }
    if addr.contains(':') {
        return addr.to_string(); // host:port, or something we should not guess at
    }
    format!("{addr}:{}", tp_net::DEFAULT_PORT)
}

pub(crate) fn run_pair_request(addr: &str) -> Result<()> {
    let app = app()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let r = rt.block_on(app.pair_request(&with_default_port(addr), tp_net::serve_port()))?;

    println!("peer  : {} ({})", r.name, r.device_id);
    println!("their side: {}", r.their_status);
    if r.my_status == tp_net::pairing::PairingStatus::Trusted {
        println!("\nThis machine ALREADY trusts that peer — nothing to approve here.");
        println!("They still have to approve us on their side if they have not.");
    } else {
        println!("\nNothing is trusted yet. On BOTH machines, compare the device ids above,");
        println!("then run:  fl pair approve <device-id>");
    }
    Ok(())
}

pub(crate) fn run_pair_list() -> Result<()> {
    let p = app()?.pairings()?;
    if p.pending.is_empty() {
        println!("no pending pairings");
    }
    for x in &p.pending {
        let dir = match x.direction {
            tp_app::Direction::TheyAskedUs => "they asked us",
            tp_app::Direction::WeAskedThem => "we asked them",
        };
        println!("{:<24} {:<14} {}", x.name, dir, x.device_id);
    }
    for x in &p.trusted {
        println!("{:<24} {:<14} {}", x.name, "trusted", x.id);
    }
    // The other half of refusing the newcomer rather than evicting the oldest
    // when the list fills: refusing is only the safer failure if it is loud.
    // A request that silently never appears here is what an attacker filling
    // the list is going for.
    let incoming = p
        .pending
        .iter()
        .filter(|x| matches!(x.direction, tp_app::Direction::TheyAskedUs))
        .count();
    if incoming >= tp_net::pairing::MAX_PENDING_IN {
        println!(
            "\n{incoming} incoming requests — the cap. No new machine can pair until some are\n\
             cleared: fl pair reject <device-id>"
        );
    }
    Ok(())
}

pub(crate) fn run_pair_decide(device_id: &str, accept: bool) -> Result<()> {
    match app()?.pair_decide(device_id, accept)? {
        Some(status) => {
            println!("{device_id} → {status:?}");
            println!("this machine will now answer that peer's signed queries");
        }
        None => println!("{device_id} refused and removed"),
    }
    Ok(())
}

pub(crate) fn run_pair_revoke(device_id: &str) -> Result<()> {
    app()?.pair_revoke(device_id)?;
    println!("{device_id} revoked and removed");
    println!("its next signed request will be refused; it is not notified");
    Ok(())
}

/// How fast this file is growing: a size says nothing without a rate. The old
/// turns are the ones with no other copy (a runtime deletes its transcripts
/// long before floonet would), so an age-based deletion policy would destroy
/// the irreplaceable and keep the redundant.
fn print_growth(app: &tp_app::App, size: u64, total: i64) -> Result<()> {
    if total == 0 {
        return Ok(());
    }
    let now = tp_core::now_ms();
    let day = 86_400_000i64;
    let (d7, d30) = (app.turns_since_days(7)?, app.turns_since_days(30)?);
    let oldest: Option<i64> = app.oldest_turn_ms()?;
    let span_days = oldest
        .map(|o| ((now - tp_core::Millis::new(o)) / day).max(1))
        .unwrap_or(1);
    let per_turn = size as f64 / total as f64;
    // The recent rate, not the lifetime mean, which cannot describe a rate
    // that is climbing.
    let per_day = (d7 as f64 / 7.0).max(d30 as f64 / 30.0);
    let per_year_gb = per_day * per_turn * 365.0 / 1e9;
    println!(
        "{total} turns over {span_days} days · {:.0}/day recently · {:.1} KB each \
         → about {per_year_gb:.0} GB/year at this rate",
        per_day,
        per_turn / 1024.0
    );
    Ok(())
}

pub(crate) fn run_verify(full: bool) -> Result<()> {
    let app = app()?;
    let path = db_path();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("index {} ({:.2} GB)", path.display(), size as f64 / 1e9);

    let (sessions, turns) = app.irreplaceable()?;
    let total: i64 = app.turn_count()?;
    if sessions > 0 {
        println!(
            "{sessions} session(s) / {turns} of {total} turn(s) exist ONLY here — their \
             transcripts are gone from disk, so this file is the last copy. \
             `fl backup <path>` while it is intact."
        );
    } else {
        println!("every indexed session still has its transcript on disk");
    }
    print_growth(&app, size, total)?;

    // Refused rather than reported wrong: `PRAGMA quick_check` cannot validate
    // the FTS index while another connection holds a write lock, and "locked"
    // is not a corruption finding. The two are acted on differently, so they
    // must not arrive as one value.
    if db_path() == tp_db::daemon_db_path() {
        if let Some(d) = app.daemon_status()? {
            if crate::cmd::read::daemon_is_live(d.pid) {
                anyhow::bail!(
                    "tpd is running (pid {}) and holds a lock, so the FTS index cannot be \
                     validated — a check that cannot run is not a clean bill of health.\n\
                     Stop it, verify, start it again:\n\
                     \tlaunchctl bootout gui/$(id -u)/io.teleport.tpd\n\
                     \ttp verify{}\n\
                     \tlaunchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.teleport.tpd.plist",
                    d.pid,
                    if full { " --full" } else { "" }
                );
            }
        }
    }

    let v = app.verify(full)?;
    println!("{}: {}", v.pragma, v.pragma_result);
    println!(
        "sessions whose turn_count disagrees with their turns: {}\n\
         turns with no session: {}\n\
         turns missing from the search index: {}",
        v.miscounted, v.orphans, v.unindexed
    );
    if !v.is_ok() {
        anyhow::bail!(
            "the index has problems. Nothing writes to it any more — floonet reads transcripts \
             where the runtime keeps them and holds no copy of its own — so these rows are \
             leftovers from an older version and no command repairs them. Sessions listed \
             above as ONLY here have no transcript either: that content is reachable only \
             from a backup, if you have one."
        );
    }
    println!("ok");
    Ok(())
}

pub(crate) fn run_backup(dest: &std::path::Path) -> Result<()> {
    // Refused rather than overwritten: the file being replaced is the previous
    // backup, and a failed VACUUM INTO onto a destroyed one leaves nothing.
    if dest.exists() {
        anyhow::bail!(
            "{} already exists — refusing to overwrite a backup. Name a new file, or remove it.",
            dest.display()
        );
    }
    let app = app()?;
    let (sessions, turns) = app.irreplaceable()?;

    let (_total, record_err) = app.backup_to(dest)?;
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if let Some(e) = record_err {
        // The backup is on disk and good; only the bookkeeping failed, which
        // is neither a failed command nor something to hide from `fl version`.
        println!("(could not record this backup: {e} — `fl version` will not know about it)");
    }

    println!(
        "wrote {} ({:.2} GB)\n{sessions} session(s) / {turns} turn(s) in it have no other copy",
        dest.display(),
        size as f64 / 1e9
    );
    Ok(())
}

/// Report or change whether this machine accepts peer connections. Every path
/// says what is true now and what still has to happen: a setting in the
/// database does not move a socket that is already bound.
pub(crate) fn run_listen(state: Option<&str>, port: Option<u16>) -> Result<()> {
    let app = app()?;
    match state {
        None => {
            let l = app.peer_listen()?;
            if l.enabled {
                println!("listening on port {} for paired peers", l.port);
            } else {
                println!("NOT listening — this machine cannot be reached by another.");
                println!("  Everything local is unaffected: mailbox, wake, search.");
                println!("  Turn it on with `fl listen on` (then restart fld), or in the panel.");
            }
            match l.configured_port {
                Some(p) => println!("  port {p} (configured)"),
                None => println!("  port {} (default)", tp_net::DEFAULT_PORT),
            }
            Ok(())
        }
        Some(s) => {
            let on = s == "on";
            app.set_peer_listen(on, port)?;
            let l = app.peer_listen()?;
            println!(
                "peer listening {} — port {}",
                if on { "ENABLED" } else { "disabled" },
                l.port
            );
            // Said every time, including when turning it off: a bound socket
            // stays bound, so "disabled" is not true of the running daemon yet.
            println!("  Takes effect when fld restarts:");
            println!("    launchctl kickstart -k gui/$(id -u)/io.teleport.tpd");
            Ok(())
        }
    }
}

#[cfg(test)]
mod default_port_tests {
    use super::with_default_port;

    #[test]
    fn a_bare_host_gets_the_port_that_discover_would_have_probed() {
        // A bare host gets the port `fl discover` would have probed.
        assert_eq!(with_default_port("192.0.2.42"), "192.0.2.42:47400");
        assert_eq!(with_default_port("mac.local"), "mac.local:47400");
    }

    #[test]
    fn an_explicit_port_is_never_second_guessed() {
        assert_eq!(with_default_port("192.0.2.42:47401"), "192.0.2.42:47401");
        assert_eq!(with_default_port("mac.local:8443"), "mac.local:8443");
        assert_eq!(with_default_port("[::1]:47401"), "[::1]:47401");
    }

    #[test]
    fn a_bare_ipv6_literal_is_bracketed_rather_than_read_as_host_colon_port() {
        // "::1" contains colons and names no port: the case that makes the
        // IpAddr parse come before the colon check.
        assert_eq!(with_default_port("::1"), "[::1]:47400");
        assert_eq!(with_default_port("fe80::1"), "[fe80::1]:47400");
    }
}
