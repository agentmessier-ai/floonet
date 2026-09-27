//! The LOCAL adapter — one JSON object per line, over a Unix socket.
//!
//! The fourth adapter over one `App`, beside `cmd/`, `mcp.rs` and the peer
//! server. It exists so the Swift panel reads daemon state through the daemon
//! instead of opening the database itself: one owner per schema. Not HTTP —
//! signatures and digests are what a network protocol needs; a local caller
//! needs a request and a reply, which is `lines()` here and `JSONDecoder` over
//! `NWEndpoint.unix(path:)` on the other side.
//!
//! Not an authorization boundary. `SO_PEERCRED` names the connecting uid; the
//! 0600 socket in a 0700 directory is what keeps other accounts out, and
//! per-message code identity would need XPC. What this adapter provides is the
//! seam: every call names a capability, so there is exactly one place a
//! decision about one could later be made.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// A request: a verb, and its arguments.
///
/// A verb rather than a path: a policy has to be expressible against a stable
/// name, and an audit line reading `machine.list` is a sentence. The names are
/// a contract — renaming one orphans every audit line already written.
#[derive(Debug, Deserialize)]
pub struct Call {
    pub capability: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Reply {
    Ok {
        ok: bool,
        result: serde_json::Value,
    },
    Err {
        ok: bool,
        /// The capability that failed, so a client can say "floonet refused X"
        /// rather than "something went wrong".
        capability: String,
        error: String,
    },
}

impl Reply {
    fn ok(result: serde_json::Value) -> Self {
        Reply::Ok { ok: true, result }
    }
    fn err(capability: &str, e: impl std::fmt::Display) -> Self {
        Reply::Err {
            ok: false,
            capability: capability.to_string(),
            error: e.to_string(),
        }
    }
}

/// Everything this adapter can be asked for. Exactly what the panel calls — a
/// capability nobody calls is a capability nobody has thought about.
pub const CAPABILITIES: &[&str] = &[
    "daemon.status",
    "live.list",
    "machine.list",
    "message.list",
    "terminal_alias.set",
    "listen.get",
    "listen.set",
    "capabilities.list",
];

fn dispatch(app: &Arc<Mutex<tp_app::App>>, call: &Call) -> Reply {
    let cap = call.capability.as_str();
    // Locked per call, released before the reply is written: the panel polls
    // while the daemon writes, so holding it across I/O would put the watcher
    // behind a socket write.
    let guard = match app.lock() {
        Ok(g) => g,
        Err(e) => return Reply::err(cap, format!("app lock poisoned: {e}")),
    };
    let out: Result<serde_json::Value> = match cap {
        "capabilities.list" => Ok(serde_json::json!({ "capabilities": CAPABILITIES })),
        // `configured_port` is reported apart from `port` so the panel can
        // show the default apart from a port the user chose. `enabled` is the
        // point: a machine that is not reachable should say so plainly rather
        // than leave it to be inferred from a pairing that never completes.
        "listen.get" => guard.peer_listen().map(|l| {
            serde_json::json!({
                "enabled": l.enabled,
                "port": l.port,
                "configured_port": l.configured_port,
                "default_port": tp_net::DEFAULT_PORT,
            })
        }),
        "listen.set" => {
            let enabled = call
                .args
                .get("enabled")
                .and_then(serde_json::Value::as_bool);
            let port = call
                .args
                .get("port")
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u16::try_from(p).ok());
            match enabled {
                Some(on) => guard.set_peer_listen(on, port).map(|()| {
                    serde_json::json!({
                        "enabled": on,
                        "port": port,
                        // Nothing rebinds a port under a running server, so
                        // the caller is told rather than left to discover it.
                        "restart_required": true,
                    })
                }),
                None => Err(anyhow::anyhow!("`enabled` (boolean) is required")),
            }
        }
        "daemon.status" => guard.daemon_status().map(|d| match d {
            Some(d) => serde_json::json!({
                "running": true, "version": d.version, "pid": d.pid,
                "started_at": d.started_at,
            }),
            // Absent is an answer, not an error: the daemon has never started.
            None => serde_json::json!({ "running": false }),
        }),
        "live.list" => guard.live().map(|rows| {
            let items = rows
                .iter()
                .map(|r| {
                    let alias = r
                        .row
                        .cwd
                        .as_deref()
                        .and_then(|c| guard.terminal_alias(c).ok().flatten());
                    serde_json::json!({
                        // The conversation address first, as everywhere else:
                        // it survives compaction, and a segment id copied from
                        // a listing does not.
                        "address": r.address,
                        "session_id": r.row.session_id,
                        "pid": r.row.pid,
                        "tty": r.row.tty,
                        "cwd": r.row.cwd,
                        "source": r.row.source,
                        "last_seen_at": r.row.last_seen_at,
                        "alias": alias,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({ "live": items })
        }),
        "machine.list" => guard.peers().map(|rows| {
            let items = rows
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id, "name": p.name, "trust": p.trust,
                        "addr": p.addr, "last_seen_at": p.last_seen_at,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({ "machines": items })
        }),
        "message.list" => {
            let limit = call
                .args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(50)
                .min(500) as usize;
            guard.recent_messages(limit).map(|msgs| {
                let items = msgs
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id, "to_session": m.to_session,
                            "from_machine": m.from_machine, "kind": m.kind,
                            "body": m.body, "created_at": m.created_at,
                            "read_at": m.read_at,
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({ "messages": items })
            })
        }
        "terminal_alias.set" => {
            let cwd = call.args.get("cwd").and_then(|v| v.as_str());
            let alias = call.args.get("alias").and_then(|v| v.as_str());
            match (cwd, alias) {
                (Some(c), Some(a)) => guard
                    .set_terminal_alias(c, a)
                    .map(|()| serde_json::json!({ "set": true })),
                _ => Err(anyhow::anyhow!("terminal_alias.set needs cwd and alias")),
            }
        }
        other => Err(anyhow::anyhow!("unknown capability {other:?}")),
    };
    match out {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(cap, e),
    }
}

/// Bind, and hand the listener back so a caller can report the real path.
///
/// A stale socket left by a crash is removed first; one something is still
/// accepting on is refused. Bind, then set the socket to 0600. The mode is set
/// after the bind rather than by narrowing the umask around it: umask is
/// process-global and would strip the execute bit from any directory another
/// thread creates in that window. The gap between bind and chmod is covered by
/// the containing directory — the daemon's socket lives in `~/.teleport`, held
/// at 0700 by tp-db. A failed chmod fails the bind: the mode is the only
/// access control.
///
/// The path is not configurable to somewhere arbitrary because a Unix socket
/// path is bounded by `SUN_LEN`.
pub fn bind(path: &std::path::Path) -> std::io::Result<UnixListener> {
    // A stale socket refuses connections; a live one accepts. Refusing here
    // costs a second daemon its local API, which the caller treats as
    // non-fatal. Stealing the path costs the first daemon its API invisibly:
    // it keeps its descriptor while every new connection is refused.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "{} is already served by a running daemon — not taking it over",
                path.display()
            ),
        ));
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve until the process ends.
pub async fn serve(listener: UnixListener, app: Arc<Mutex<tp_app::App>>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tp_core::log_warn!("local api: accept failed: {e}");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            // A malformed line is answered and the connection continues: the
            // panel holds one connection for its whole run, and dropping it on
            // one bad frame would take the working calls with it.
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let reply = match serde_json::from_str::<Call>(&line) {
                    Ok(call) => dispatch(&app, &call),
                    Err(e) => Reply::err("(unparsed)", e),
                };
                let mut buf = match serde_json::to_vec(&reply) {
                    Ok(b) => b,
                    Err(e) => {
                        tp_core::log_warn!("local api: cannot serialise reply: {e}");
                        continue;
                    }
                };
                buf.push(b'\n');
                if w.write_all(&buf).await.is_err() {
                    return;
                }
            }
        });
    }
}
