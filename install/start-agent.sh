#!/usr/bin/env bash
# Start (or restart) the fld LaunchAgent. Sourced by install.sh; separate so it
# can be driven by a test with a stand-in `launchctl` on PATH.
#
#   start_agent LABEL PLIST
#
# Returns 0 when the agent is running, 2 when it was deliberately not started,
# 1 when starting it failed.
start_agent() {
    local label="$1" plist="$2" uid
    uid="$(id -u)"
    if ! launchctl print "gui/$uid" >/dev/null 2>&1; then
        echo "fld installed but NOT started: no one is logged in at this machine's screen,"
        echo "  so there is no gui/$uid domain to load it into yet."
        echo "  It will start by itself at the next login on this machine's screen."
        return 2
    fi
    # Reinstalling over a running agent: unload first, or bootstrap reports
    # "service already loaded" and the old binary keeps running.
    launchctl bootout "gui/$uid/$label" 2>/dev/null || true
    # A disabled service refuses to load until it is enabled, and the flag
    # outlives uninstall and reboot. launchd reports it only as "5: Input/output
    # error", which reads as a session problem and was diagnosed as one. Running
    # this installer is the request to run the agent, so the flag is cleared.
    launchctl enable "gui/$uid/$label" 2>/dev/null || true
    launchctl bootstrap "gui/$uid" "$plist" 2>&1
    sleep 1
    if launchctl print "gui/$uid/$label" >/dev/null 2>&1; then
        echo "fld is running. Logs: ~/.teleport/tpd.err.log"
        return 0
    fi
    echo "fld failed to start. launchd's own error, if any, is above;" >&2
    echo "  its state for this service: $(launchctl print-disabled "gui/$uid" 2>/dev/null \
        | grep -F "\"$label\"" | sed 's/^[[:space:]]*//' || echo unknown)" >&2
    echo "  and the daemon's own log: ~/.teleport/tpd.err.log" >&2
    return 1
}
