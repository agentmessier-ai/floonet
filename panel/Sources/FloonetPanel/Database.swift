import Foundation

struct LiveSession: Identifiable {
    let session_id: String
    let pid: Int32
    let tty: String?
    let cwd: String?
    let source: String
    let last_seen_at: Int64
    let alias: String?

    var id: String { session_id }
    var displayAlias: String {
        if let a = alias, !a.isEmpty { return a }
        if let cwd = cwd, !cwd.isEmpty { return (cwd as NSString).lastPathComponent }
        return session_id
    }
    var runtimeLabel: String {
        let parts = session_id.split(separator: "/", maxSplits: 2)
        return parts.count >= 2 ? String(parts[1]) : "?"
    }
    var isHook: Bool { source == "hook" }
}

struct Peer: Identifiable {
    let id: String
    let name: String
    let trust: String
    let addr: String?
    let last_seen_at: Int64?
}

struct PokeLog: Identifiable {
    let id: String
    let to_session: String
    let from_machine: String
    let kind: String
    let body: String
    let created_at: Int64
    let read_at: Int64?
}

/// Which build each of the three separately-installed pieces is.
///
/// They drift, routinely and silently: installing binaries does not restart
/// the LaunchAgent, and the panel is a bundle installed by a different command
/// again. `running` is the only one that describes the process actually
/// serving peer requests, which is why a mismatch against `cli` is worth
/// saying out loud rather than leaving for someone to notice.
struct Versions {
    var running: String?
    var startedAt: Int64?
    var cli: String?
    var panel: String

    /// Mirrors `tp_core::compare_builds` — three outcomes, not a bool. A dirty
    /// tree cannot be compared at all, so warning on one would nag through
    /// every rebuild while a genuinely stale daemon looks identical.
    enum Match { case same, different, unknown }

    var daemonMatch: Match {
        guard let a = sha(running), let b = sha(cli) else { return .unknown }
        if a.contains("dirty") || b.contains("dirty") || a == "unknown" || b == "unknown" {
            return .unknown
        }
        return a == b ? .same : .different
    }

    /// The commit out of `0.1.0 (33349da, 2026-08-16)`.
    private func sha(_ line: String?) -> String? {
        guard let line,
              let open = line.firstIndex(of: "("),
              let comma = line[line.index(after: open)...].firstIndex(where: { $0 == "," || $0 == ")" })
        else { return nil }
        return String(line[line.index(after: open)..<comma])
    }
}

/// Whether other machines can reach this one, and on what port.
///
/// Shown even when off — especially when off. floonet is a local program by
/// default, and a user who expects their laptop to answer a paired machine has
/// no way to discover that it never opened a port unless something says so.
struct PeerListen {
    var enabled = false
    var port = 47400
    /// nil means the default is in force rather than a port the user chose.
    var configuredPort: Int?
}

struct PanelState {
    var sessions: [LiveSession] = []
    var peers: [Peer] = []
    var recentPokes: [PokeLog] = []
    var tpdRunning: Bool = false
    var listen = PeerListen()
    var versions = Versions(panel: FloonetAPI.panelVersion())
}

final class FloonetAPI {
    static let flBin = "\(NSString(string: "~/.local/bin").expandingTildeInPath)/fl"

    /// Everything the panel shows, in four calls to the daemon.
    ///
    /// It used to open `teleport.db` directly. There is deliberately NO
    /// fallback to that path: the daemon is what reconciles `live_session`, so
    /// rows read while it is down are stale by definition, and presenting them
    /// beside a "not running" badge is how a panel shows a session that ended
    /// an hour ago. `tpdRunning == false` with empty lists is the honest
    /// rendering, and the restart button is right there.
    ///
    /// Versions are the exception and still resolve: `cli` is the binary on
    /// disk and `panel` is this bundle, neither of which needs a daemon. That
    /// is the comparison the user needs most when the daemon is the thing that
    /// is wrong.
    func loadState() -> PanelState {
        var state = PanelState()

        // One round trip decides it. A daemon that answers a capability is
        // running in the sense the panel cares about — `lsof` on the TCP port
        // could not tell a serving daemon from a wedged one holding the socket.
        if let status = LocalAPI.call("daemon.status") {
            state.tpdRunning = status["running"] as? Bool ?? false
            state.versions.running = status["version"] as? String
            state.versions.startedAt = status["started_at"] as? Int64
        } else {
            state.versions.cli = Self.cliVersion()
            return state
        }

        if let l = LocalAPI.call("listen.get") {
            state.listen = PeerListen(
                enabled: l["enabled"] as? Bool ?? false,
                port: l["port"] as? Int ?? 47400,
                configuredPort: l["configured_port"] as? Int
            )
        }

        if let r = LocalAPI.call("live.list"), let rows = r["live"] as? [[String: Any]] {
            state.sessions = rows.map {
                LiveSession(
                    session_id: $0["session_id"] as? String ?? "",
                    pid: Int32($0["pid"] as? Int ?? 0),
                    tty: $0["tty"] as? String,
                    cwd: $0["cwd"] as? String,
                    source: $0["source"] as? String ?? "scan",
                    last_seen_at: ($0["last_seen_at"] as? NSNumber)?.int64Value ?? 0,
                    alias: $0["alias"] as? String
                )
            }
        }

        if let r = LocalAPI.call("machine.list"), let rows = r["machines"] as? [[String: Any]] {
            state.peers = rows.map {
                Peer(
                    id: $0["id"] as? String ?? "",
                    name: $0["name"] as? String ?? "",
                    trust: $0["trust"] as? String ?? "",
                    addr: $0["addr"] as? String,
                    last_seen_at: ($0["last_seen_at"] as? NSNumber)?.int64Value
                )
            }
        }

        if let r = LocalAPI.call("message.list", args: ["limit": 12]),
            let rows = r["messages"] as? [[String: Any]]
        {
            state.recentPokes = rows.map {
                PokeLog(
                    id: $0["id"] as? String ?? "",
                    to_session: $0["to_session"] as? String ?? "",
                    from_machine: $0["from_machine"] as? String ?? "",
                    kind: $0["kind"] as? String ?? "",
                    body: $0["body"] as? String ?? "",
                    created_at: ($0["created_at"] as? NSNumber)?.int64Value ?? 0,
                    read_at: ($0["read_at"] as? NSNumber)?.int64Value
                )
            }
        }

        state.versions.cli = Self.cliVersion()
        return state
    }

    /// This bundle's own version, stamped into Info.plist at `make bundle`.
    static func panelVersion() -> String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
    }

    /// The version of the `tp` binary ON DISK — deliberately not the same
    /// question as what the daemon is running, which is the whole point of
    /// comparing them.
    private static func cliVersion() -> String? {
        let out = runFl(["--version"])
        guard !out.isEmpty, !out.hasPrefix("failed to run tp") else { return nil }
        // "fl 0.2.1 (33349da, 2026-08-17)" → drop the leading binary name.
        return out.hasPrefix("fl ") ? String(out.dropFirst(3)) : out
    }

    /// The panel's ONE write, and it no longer reaches the file. The table it
    /// targets is declared by `migrations/0004_panel.sql`; this code used to
    /// create it as well.
    func setAlias(cwd: String, alias: String) {
        _ = LocalAPI.call("terminal_alias.set", args: ["cwd": cwd, "alias": alias])
    }

    /// Turn the peer port on or off, then restart the daemon so it takes.
    ///
    /// The restart is not optional and not hidden: nothing rebinds a port under
    /// a running server, so a panel that flipped the switch and left the daemon
    /// alone would show "on" beside a machine still refusing connections.
    static func setListen(enabled: Bool, port: Int?) {
        var args: [String: Any] = ["enabled": enabled]
        if let p = port { args["port"] = p }
        _ = LocalAPI.call("listen.set", args: args)
        restartTpd()
    }

    static func restartTpd() {
        let uid = getuid()
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        task.arguments = ["kickstart", "-k", "gui/\(uid)/io.teleport.tpd"]
        try? task.run()
    }

    /// Enqueue a message and report what actually happened to it.
    ///
    /// This used to spawn `tp ask`, pipe both streams into a `Pipe` it never
    /// read, and not wait for the process — so the panel said "sent" for every
    /// outcome, including the ones `tp` names precisely: a target that is
    /// registered but has no injectable pane comes back "registered but not
    /// injectable — target checks on next /tp inbox", and a session in a
    /// terminal with no backend can only ever come back that way. Reported by
    /// the operator as "I poked it and it never arrived" — which was true, and
    /// the panel was the only component that did not know.
    static func poke(sessionId: String, message: String) -> String {
        runFl(["ask", sessionId, message])
    }

    /// Runs `tp <args>` and returns its combined stdout+stderr, trimmed.
    ///
    /// Every panel action goes through here: `tp` reports outcomes as text on
    /// those streams — "already trusted", "not trusted", "registered but not
    /// injectable" — and a panel that discards them is a panel that invents a
    /// result it did not get.
    private static func runFl(_ args: [String]) -> String {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: flBin)
        task.arguments = args
        let out = Pipe()
        let err = Pipe()
        task.standardOutput = out
        task.standardError = err
        do {
            try task.run()
            task.waitUntilExit()
            let outText = String(data: out.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            let errText = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            return (outText + errText).trimmingCharacters(in: .whitespacesAndNewlines)
        } catch {
            return "failed to run tp: \(error.localizedDescription)"
        }
    }

    static func pairApprove(id: String) -> String { runFl(["pair", "approve", id]) }
    static func pairReject(id: String) -> String { runFl(["pair", "reject", id]) }
    static func pairRevoke(id: String) -> String { runFl(["pair", "revoke", id]) }

    static func focusTerminal(tty: String) {
        let ttyName = tty.trimmingCharacters(in: .whitespaces).replacingOccurrences(of: "/dev/", with: "")
        let script = """
        tell application "iTerm2"
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        try
                            if (tty of s) ends with "\(ttyName)" then
                                set index of w to 1
                                select t
                                activate
                                return
                            end if
                        end try
                    end repeat
                end repeat
            end repeat
        end tell
        """
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        task.arguments = ["-e", script]
        try? task.run()
    }
}
