//! `wake()`'s caller-capability gate: `Target::Terminal` requires the calling
//! process to hold the Automation TCC grant, which `fld` (a bare LaunchAgent)
//! never has, so it is refused explicitly rather than discovered at runtime.

#[test]
fn daemon_caller_refuses_iterm_backend() {
    let target = tp_reach::Target::Terminal {
        id: "iterm2".into(),
        tty: "/dev/ttys999".into(),
    };
    let err = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    )
    .unwrap_err();
    assert!(err.to_string().contains("TCC-blocked"), "got: {err}");
}

#[test]
fn cli_caller_is_allowed_to_attempt_iterm_backend() {
    // No real session at this tty, so this exercises "allowed to try, fails
    // because the target does not exist", not "refused outright".
    let target = tp_reach::Target::Terminal {
        id: "iterm2".into(),
        tty: "/dev/ttys999-does-not-exist".into(),
    };
    let err = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Cli,
    )
    .unwrap_err();
    assert!(
        !err.to_string().contains("TCC-blocked"),
        "Cli caller must not hit the daemon-only refusal, got: {err}"
    );
}

#[test]
fn unreachable_and_not_live_targets_are_always_a_silent_no_op() {
    // Mailbox-only degradation is intentional, for either caller.
    tp_reach::wake(
        &tp_reach::Target::Unreachable,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Cli,
    )
    .unwrap();
    tp_reach::wake(
        &tp_reach::Target::NotLive,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    )
    .unwrap();
}

/// A wake into a pane that is gone must fail, not report success: `?` on a
/// `Command` only propagates a spawn failure, and a non-zero exit reported as
/// `Woke` would count an attempt against `MAX_DELIVER` for a message nothing
/// received.
#[test]
fn a_wake_into_a_dead_tmux_pane_is_an_error_not_a_wake() {
    // A pane id no server will have. If tmux is not running the command still
    // exits non-zero: the target is not there either way.
    let target = tp_reach::Target::Tmux("%999999".into());
    let got = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Cli,
    );
    let err = got.expect_err("sending to a pane that does not exist must not report success");
    let msg = err.to_string();
    assert!(
        msg.contains("tmux send-keys failed"),
        "the error must name the mechanism that failed, got: {msg}"
    );
    assert!(
        msg.contains("%999999"),
        "and the target it failed against, got: {msg}"
    );
}

/// A 200 that arrives in pieces is still a 200: TCP does not promise a whole
/// status line per read, even on loopback. This server writes the status line
/// one byte at a time with a flush between, which forces the split.
#[test]
fn a_status_line_split_across_tcp_segments_is_still_read() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut scratch = [0u8; 1024];
        let _ = sock.read(&mut scratch); // drain the request
                                         // Writes are best-effort: the client stops reading at the end of the
                                         // status line and closes, so the remaining bytes may hit a broken pipe.
        for b in b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n" {
            if sock.write_all(&[*b]).is_err() || sock.flush().is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    });

    let target = tp_reach::Target::Channel(tp_reach::DeliveryChannel::Http(format!(
        "http://127.0.0.1:{port}/wake"
    )));
    let got = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    );
    server.join().unwrap();
    assert!(
        got.is_ok(),
        "a byte-at-a-time 200 must be read as delivered, got: {:?}",
        got.err()
    );
}

/// And a real failure still fails: the reader must not become permissive.
#[test]
fn a_channel_answering_500_is_not_a_wake() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut scratch = [0u8; 1024];
        let _ = sock.read(&mut scratch);
        let _ = sock.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
    });
    let target = tp_reach::Target::Channel(tp_reach::DeliveryChannel::Http(format!(
        "http://127.0.0.1:{port}/wake"
    )));
    let got = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    );
    server.join().unwrap();
    assert!(got.is_err(), "a 500 must not be reported as delivered");
}

/// A channel URL's query string reaches the listener, and the request target
/// keeps its leading slash.
///
/// The dsh plugin authenticates its wake route with a per-session token it
/// carries in the channel URL, so the query is load-bearing rather than
/// decoration: splitting the host off without putting the `/` back, or
/// dropping everything after `?`, turns every wake into a 403 that looks like
/// a dead session.
#[test]
fn a_channel_url_keeps_its_query_string_and_leading_slash() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut scratch = [0u8; 1024];
        let n = sock.read(&mut scratch).unwrap_or(0);
        let _ =
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        String::from_utf8_lossy(&scratch[..n]).to_string()
    });

    let target = tp_reach::Target::Channel(tp_reach::DeliveryChannel::Http(format!(
        "http://127.0.0.1:{port}/floonet/wake?token=s3cret"
    )));
    let got = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    );
    let request = server.join().unwrap();
    assert!(got.is_ok(), "wake failed: {:?}", got.err());
    let line = request.lines().next().unwrap_or_default();
    assert_eq!(
        line, "POST /floonet/wake?token=s3cret HTTP/1.1",
        "the request target must keep both the leading slash and the query"
    );
}
