//! What the pi extension does when pi runs headless.
//!
//! A data pipeline running `pi -p --no-session --mode json` with sixteen
//! workers kept a dozen pi "sessions" in `fl live` around the clock: each
//! one-shot call loaded the extension, and `session_start` registered it. None
//! had a terminal to wake, and `--no-session` meant none had a transcript to
//! read — addresses that could neither be messaged nor read, crowding the list
//! senders choose from. pi says which mode it is in; print and json are the
//! one-shot modes ("extensions run but can't prompt").

use std::path::PathBuf;

fn extension() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../integrations/pi/floonet.ts");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The `session_start` handler, up to its `register` call.
fn before_register(src: &str) -> &str {
    let start = src
        .find("pi.on(\"session_start\"")
        .expect("the extension handles session_start");
    let reg = src[start..]
        .find("\"register\"")
        .expect("session_start calls `fl register`");
    &src[start..start + reg]
}

#[test]
fn a_one_shot_pi_run_is_not_registered_as_a_live_session() {
    let src = extension();
    let guard = before_register(&src);
    assert!(
        guard.contains("ctx.mode") && guard.contains("\"print\"") && guard.contains("\"json\""),
        "session_start must skip registration in print and json mode before calling `fl register`:\n{guard}"
    );
}

/// The interactive and RPC modes are conversations a person is having, and
/// stay reachable. Excluding them would make the guard above a way to turn
/// reach off entirely.
#[test]
fn the_interactive_modes_still_register() {
    let src = extension();
    let guard = before_register(&src);
    for mode in ["\"tui\"", "\"rpc\""] {
        assert!(
            !guard.contains(mode),
            "{mode} must not be among the modes that skip registration:\n{guard}"
        );
    }
}
