import Foundation
import Network

/// The panel's way in: one JSON object per line, over `~/.teleport/tpd.sock`.
///
/// It replaced direct SQLite access, and the reason is not tidiness. The panel
/// opened `teleport.db` `SQLITE_OPEN_READWRITE` and ran
/// `CREATE TABLE IF NOT EXISTS terminal_alias` for a table
/// `migrations/0004_panel.sql` already declares — two owners for one schema,
/// which is how the two drift. Everything it needs is now a verb the daemon
/// answers, and the daemon is the only writer.
///
/// `NWConnection` rather than `socket(2)`: `NWEndpoint.unix(path:)` has been
/// public since macOS 10.14, so none of this is hand-rolled POSIX.
///
/// SYNCHRONOUS on purpose. The caller is a 2-second poll on a background queue
/// that previously blocked on SQLite; making it async would push Swift
/// concurrency through `PanelState` and `ContentView` for no gain the user can
/// see. A request that does not answer inside `timeout` is treated as no
/// daemon, which is the same conclusion the old code reached when the file was
/// unreadable.
final class LocalAPI {
    static let socketPath = NSString(string: "~/.teleport/tpd.sock").expandingTildeInPath

    /// Bounded by the POLL INTERVAL, not by how slow a daemon might be.
    ///
    /// The failure that sets this is a stale socket — a file left behind by a
    /// daemon that crashed, which accepts nothing. Measured: three calls
    /// against one of those took 4.5s at a 1500ms timeout, and the panel polls
    /// every 2s, so refreshes would pile up on each other. `loadState` returns
    /// after the FIRST failure, so the worst case is one of these; 600ms keeps
    /// that comfortably inside a cycle while still crossing a daemon busy with
    /// a peer scan (the local adapter holds the lock only per call, never
    /// across a search).
    private static let timeout: DispatchTimeInterval = .milliseconds(600)

    /// One request, one reply. Returns `nil` when there is no daemon to ask —
    /// which the caller must render as "not running" rather than as empty data,
    /// because those are different facts.
    static func call(_ capability: String, args: [String: Any] = [:]) -> [String: Any]? {
        guard FileManager.default.fileExists(atPath: socketPath) else { return nil }

        var request: [String: Any] = ["capability": capability]
        if !args.isEmpty { request["args"] = args }
        guard var body = try? JSONSerialization.data(withJSONObject: request) else { return nil }
        body.append(0x0A)  // newline: the frame delimiter

        let conn = NWConnection(to: .unix(path: socketPath), using: .tcp)
        let done = DispatchSemaphore(value: 0)
        var reply: [String: Any]?
        // Replies are read incrementally because a receive can return a partial
        // frame; the line, not the packet, is the unit.
        var buffer = Data()

        func readMore() {
            conn.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) { data, _, isDone, error in
                if let d = data { buffer.append(d) }
                if let nl = buffer.firstIndex(of: 0x0A) {
                    let line = buffer[buffer.startIndex..<nl]
                    reply = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any]
                    done.signal()
                    return
                }
                if isDone || error != nil {
                    done.signal()
                    return
                }
                readMore()
            }
        }

        conn.stateUpdateHandler = { state in
            switch state {
            case .ready:
                conn.send(content: body, completion: .contentProcessed { _ in })
                readMore()
            case .failed, .cancelled:
                done.signal()
            default:
                break
            }
        }
        conn.start(queue: .global(qos: .utility))
        _ = done.wait(timeout: .now() + timeout)
        conn.cancel()

        // `ok: false` is a REPLY, not an absence: the daemon answered and
        // refused. Returning nil for it would make a refused capability look
        // like a dead daemon, and the panel would offer to restart something
        // that is running fine.
        guard let r = reply else { return nil }
        if r["ok"] as? Bool == true { return r["result"] as? [String: Any] }
        return r
    }

    /// `true` when the daemon answered anything at all.
    ///
    /// Replaces shelling out to `lsof -iTCP:47400`, which asked a related but
    /// different question — a daemon can hold the TCP port while being wedged,
    /// and `lsof` on a busy machine is slower than this whole round trip.
    static func daemonAnswers() -> Bool {
        call("capabilities.list") != nil
    }
}
