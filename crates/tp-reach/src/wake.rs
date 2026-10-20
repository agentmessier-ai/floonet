//! The security-critical injection path: `wake()` only ever types
//! a fixed control string into another session's pane. Message content stays
//! in the DB; the target reads it via `/fl inbox`. A hostile peer can at most
//! make the target *check its inbox* — never execute arbitrary content.
//!
//! `type_raw()` is the escape hatch for targets with no `/fl inbox`
//! equivalent (another agent CLI that just reads keyboard input). It has none
//! of the above guarantee.

use crate::resolve::Target;
use crate::terminal;
use anyhow::{bail, Context, Result};

/// The default phrase typed into a peer pane, for a runtime that declares
/// none. The phrase is a property of the runtime, not of floonet (a runtime
/// without this slash command would reject it), so a runtime may declare its
/// own and the caller resolves it: `tp-reach` holds no runtime knowledge.
/// The pane invariant holds either way: the phrase comes from an
/// operator-controlled descriptor, and no part of it derives from the request.
pub const CONTROL_STRING: &str = "/fl inbox";

/// Who is calling `wake()`; decides which backends are even attempted.
/// `Target::Terminal` requires the AppleScript-driving process to hold the
/// Automation TCC grant. A CLI launched inside a terminal inherits it through
/// the responsible-process chain; `fld`, a bare LaunchAgent, has no such chain
/// and must never attempt it, expressed as a precondition rather than a
/// runtime failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A CLI process invoked directly or as a subprocess of the terminal
    /// (`fl ask`, `fl mcp`). May use any backend, including AppleScript.
    Cli,
    /// `fld`, the background LaunchAgent. TCC-blocked from AppleScript by
    /// construction; restricted to TCC-free backends.
    Daemon,
}

/// Type the control string into the target. `Target::Unreachable` /
/// `Target::NotLive` are no-ops (mailbox-only delivery).
pub fn wake(target: &Target, session_id: &str, control: &str, caller: Caller) -> Result<()> {
    type_text(target, control, session_id, caller)
}

/// Type arbitrary text directly into the target pane. Not the safe path: it
/// exists only for targets with no `/fl inbox` equivalent to dereference
/// through. Whatever is passed lands in the target's input as if typed, with
/// none of `wake()`'s guarantee that only a fixed string crosses the pane
/// boundary, and no confirmation gate on the receiving end.
pub fn type_raw(target: &Target, text: &str, caller: Caller) -> Result<()> {
    // No session id: `type_raw` is the pane-only escape hatch and never targets
    // a channel, whose whole purpose is addressing one session.
    type_text(target, text, "", caller)
}

/// Hand the control string to a runtime-declared channel. Hand-rolled rather
/// than pulling an HTTP client into this crate: the request is one fixed
/// shape to a loopback address, and an HTTP client would drag an async
/// runtime into a synchronous crate. `DeliveryChannel::parse` has already
/// refused anything non-loopback.
fn deliver_via_channel(
    chan: &crate::resolve::DeliveryChannel,
    text: &str,
    session_id: &str,
) -> Result<()> {
    use crate::resolve::DeliveryChannel;
    use std::io::Write;

    match chan {
        DeliveryChannel::Exec(argv) => {
            let mut child = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                // The receiver has to know which session to drain; a
                // multiplexed host serves many at once.
                .arg(session_id)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .with_context(|| format!("spawning delivery channel {:?}", argv[0]))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            // Wait: a channel that fails must surface as a failed wake, not as a
            // silently orphaned process reported as success.
            let status = child.wait()?;
            if !status.success() {
                bail!("delivery channel {:?} exited with {status}", argv[0]);
            }
            Ok(())
        }
        DeliveryChannel::Http(url) => {
            let (host, port, path) = split_loopback_url(url)?;
            let body = format!(
                "{{\"session_id\":{},\"control\":{}}}",
                json_string(session_id),
                json_string(text)
            );
            let req = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let addr = format!("{host}:{port}");
            let mut stream = std::net::TcpStream::connect(&addr)
                .with_context(|| format!("connecting to delivery channel {addr}"))?;
            // Bounded: a wedged listener must not hang the daemon's delivery loop.
            stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            stream.write_all(req.as_bytes())?;
            // Read until the status line is complete, not until one syscall
            // returns: TCP may deliver a partial line. Bounded three ways so a
            // wedged listener cannot hang the delivery loop: the read timeout
            // above, a 64-byte ceiling, and stopping at the first CR or LF.
            use std::io::Read;
            let mut head = Vec::with_capacity(64);
            let mut byte = [0u8; 1];
            while head.len() < 64 {
                match stream.read(&mut byte) {
                    Ok(0) => break, // peer closed; judge what we have
                    Ok(_) => {
                        if byte[0] == b'\r' || byte[0] == b'\n' {
                            break;
                        }
                        head.push(byte[0]);
                    }
                    Err(_) => break, // timeout or reset; same
                }
            }
            let status_line = String::from_utf8_lossy(&head);
            // Treat any 2xx as delivered; the body is not part of the contract.
            if !status_line
                .split(' ')
                .nth(1)
                .is_some_and(|c| c.starts_with('2'))
            {
                bail!("delivery channel {url} answered {:?}", status_line.trim());
            }
            Ok(())
        }
    }
}

/// `http://127.0.0.1:8125/path` → `("127.0.0.1", 8125, "/path")`.
fn split_loopback_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| anyhow::anyhow!("not an http url: {url}"))?;
    let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("delivery channel needs an explicit port: {url}"))?;
    Ok((
        host.to_string(),
        port.parse().with_context(|| format!("bad port in {url}"))?,
        format!("/{path}"),
    ))
}

/// JSON string escaping. The control string is descriptor-supplied text, not a
/// constant, so the escaping carries the body's validity: JSON forbids a raw
/// C0 control, and one would make the runtime reject the whole request.
fn json_string(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Types `text` into a tmux pane and submits it.
///
/// `run` executes one tmux invocation. A parameter so a test can record the
/// calls without a tmux server, or a `PATH` the rest of the suite shares.
fn tmux_type(
    pane: &str,
    text: &str,
    run: &mut dyn FnMut(&[&str]) -> std::io::Result<std::process::Output>,
) -> Result<()> {
    // Two sends with a pause between, not one. Measured from an empty prompt:
    // text and Enter in a single `send-keys` left the text unsubmitted in both
    // Claude Code and codex, because the Enter lands inside the same burst and
    // the TUI takes it as part of a paste. Codex swallowed it even from a
    // separate send with no pause; 200ms submitted in both. The failure is
    // silent on this side — tmux accepts the keys — so a wake reported
    // success while the target never read its inbox.
    //
    // `-l` so the text is typed literally: without it tmux reads an argument
    // that happens to be a key name ("Enter", "C-c") as that key.
    for (keys, settle) in [
        (&["-l", text][..], Some(ENTER_SETTLE)),
        (&["Enter"][..], None),
    ] {
        let mut args = vec!["send-keys", "-t", pane];
        args.extend_from_slice(keys);
        // `?` alone only propagates a spawn failure. A non-zero exit (the pane
        // closed since resolve, tmux gone) must be a failed wake, or a wake
        // into nothing burns a delivery attempt against MAX_DELIVER — and an
        // Enter after a failed text send would submit whatever was half-typed.
        let out = run(&args)?;
        if !out.status.success() {
            bail!(
                "tmux send-keys failed for pane {pane} (closed since resolve?): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if let Some(d) = settle {
            std::thread::sleep(d);
        }
    }
    Ok(())
}

/// How long typed text is left to settle before Enter is sent. See
/// `tmux_type` for the measurement behind the number.
const ENTER_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

fn type_text(target: &Target, text: &str, session_id: &str, caller: Caller) -> Result<()> {
    match target {
        Target::Tmux(pane) => tmux_type(pane, text, &mut |args| {
            std::process::Command::new("tmux").args(args).output()
        })?,
        // A declared channel has no TCC dimension (a subprocess spawn or a
        // loopback write, not AppleScript), so unlike `Terminal` it is
        // reachable from `fld` as well as from a CLI process.
        Target::Channel(chan) => deliver_via_channel(chan, text, session_id)?,
        Target::Terminal { id, tty } => {
            if caller == Caller::Daemon {
                // Fail loud: a daemon-side caller reaching this branch is a bug
                // in target resolution or caller wiring, not a degraded outcome.
                bail!("refusing AppleScript injection from the daemon path (TCC-blocked by design — a LaunchAgent holds no Automation grant)");
            }
            terminal_write_text(id, tty, text)?;
        }
        _ => {}
    }
    Ok(())
}

/// Type `text` into whichever object the `id` terminal owns for `tty`. The
/// script is generated from that terminal's descriptor
/// (`terminal::TerminalConfig::applescript_send`), so the quoting hazards
/// live in `terminal` with tests. Accepts either `ttysNNN` or `/dev/ttysNNN`;
/// `terminal::needle` normalizes.
fn terminal_write_text(id: &str, tty: &str, text: &str) -> Result<()> {
    let Some(cfg) = terminal::all().into_iter().find(|c| c.id == id) else {
        // The descriptor was there at resolve time and is not now (a config
        // file edited mid-flight). Say which one, not "the terminal closed".
        bail!("terminal backend {id:?} is no longer configured");
    };
    let Some(script) = cfg.applescript_send(tty, text) else {
        bail!("terminal backend {id:?} cannot inject (not an applescript backend)");
    };
    run_osascript(id, tty, &script)?;

    // The submit, as its own write (see `SUBMIT_DELAY`). An empty body puts a
    // lone newline into the pane and needs no second template.
    std::thread::sleep(SUBMIT_DELAY);
    let Some(submit) = cfg.applescript_submit(tty) else {
        bail!("terminal backend {id:?} cannot inject (not an applescript backend)");
    };
    run_osascript(id, tty, &submit)
}

/// How long to wait between typing the text and sending the newline that
/// submits it. Claude Code treats a large chunk of characters arriving at
/// once as a paste, whose trailing newline is inserted rather than submitted,
/// so the newline must arrive as its own chunk. The delay is far beyond any
/// input-coalescing window and invisible next to the two `osascript` spawns
/// around it. tmux does not need this: `send-keys … Enter` is already a
/// separate key event.
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

fn run_osascript(id: &str, tty: &str, script: &str) -> Result<()> {
    let out = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()?;
    if !out.status.success() {
        bail!("osascript failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    if String::from_utf8_lossy(&out.stdout).trim() != "ok" {
        bail!("no {id} window found with tty {tty} (closed since resolve?)");
    }
    Ok(())
}

#[cfg(test)]
mod json_tests {
    use super::json_string;

    /// Every C0 control has to leave as an escape: JSON forbids a raw one, and
    /// a descriptor author supplies the control string.
    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(json_string("a\rb"), r#""a\rb""#);
        assert_eq!(json_string("a\tb"), r#""a\tb""#);
        assert_eq!(json_string("a\u{8}b\u{c}"), r#""a\bb\f""#);
        assert_eq!(json_string("a\u{1}b\u{1f}"), r#""a\u0001b\u001f""#);
        assert_eq!(json_string("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }
}

#[cfg(test)]
mod tmux_tests {
    use super::tmux_type;
    use std::os::unix::process::ExitStatusExt as _;
    use std::time::{Duration, Instant};

    type Calls = std::rc::Rc<std::cell::RefCell<Vec<(Vec<String>, Instant)>>>;

    /// Records every invocation and the moment it happened.
    fn recorder(
        fail_first: bool,
    ) -> (
        Calls,
        impl FnMut(&[&str]) -> std::io::Result<std::process::Output>,
    ) {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = calls.clone();
        let run = move |args: &[&str]| {
            let n = log.borrow().len();
            log.borrow_mut()
                .push((args.iter().map(|a| a.to_string()).collect(), Instant::now()));
            let code = if fail_first && n == 0 { 1 << 8 } else { 0 };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(code),
                stdout: Vec::new(),
                stderr: b"can't find pane: %9".to_vec(),
            })
        };
        (calls, run)
    }

    /// Measured on two TUIs in tmux, from an empty prompt each time: text and
    /// Enter in ONE `send-keys` left the text in the prompt unsubmitted in both
    /// Claude Code and codex — the Enter arrives inside the same burst and is
    /// taken as part of a paste. That is a wake that reports success while the
    /// target never reads its inbox.
    #[test]
    fn the_text_and_the_enter_are_separate_sends() {
        let (calls, mut run) = recorder(false);
        tmux_type("%3", "Check your inbox", &mut run).unwrap();
        let calls = calls.borrow();
        let argv: Vec<&Vec<String>> = calls.iter().map(|(a, _)| a).collect();
        assert_eq!(
            argv,
            vec![
                &vec!["send-keys", "-t", "%3", "-l", "Check your inbox"]
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>(),
                &vec!["send-keys", "-t", "%3", "Enter"]
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>(),
            ],
            "text literally (-l, so a word like `Enter` in it is never read as a key), then Enter on its own"
        );
    }

    /// Separate sends were not enough for codex: with no pause between them it
    /// still swallowed the Enter. 200ms submitted in both TUIs on the slowest
    /// machine tested.
    #[test]
    fn the_enter_waits_for_the_text_to_settle() {
        let (calls, mut run) = recorder(false);
        tmux_type("%3", "x", &mut run).unwrap();
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2, "{calls:?}");
        let gap = calls[1].1.duration_since(calls[0].1);
        assert!(
            gap >= Duration::from_millis(200),
            "Enter followed the text after {gap:?}; codex takes that as part of the paste"
        );
    }

    /// Enter into a pane whose text never arrived would submit whatever the
    /// user had half-typed there.
    #[test]
    fn a_failed_text_send_sends_no_enter_and_names_the_pane() {
        let (calls, mut run) = recorder(true);
        let err = tmux_type("%9", "x", &mut run).unwrap_err().to_string();
        assert_eq!(calls.borrow().len(), 1, "no Enter after a failed send");
        assert!(
            err.contains("tmux send-keys failed") && err.contains("%9"),
            "{err}"
        );
    }
}
