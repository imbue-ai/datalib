//! Ctrl-C ends `datalib-http` cleanly while a browser tab holds
//! `/api/sync/stream` open: the stream ends on shutdown, so graceful
//! shutdown finishes and stops the applets and the loop. A connection
//! that still never ends is cut off by the deadline in `main.rs`.

mod server;

use std::io::Read;

use server::start;

fn root() -> tempfile::TempDir {
    tempfile::TempDir::with_prefix("datalib-http-shutdown-itest-").expect("tempdir")
}

/// Before the stream ended on shutdown, an open tab held graceful
/// shutdown until the deadline cut it off, and the applets and the
/// loop's steps were never stopped.
#[test]
fn sigint_ends_an_open_sse_tail_and_shuts_down_gracefully() {
    let dir = root();
    let mut server = start(&dir.path().join("data"));
    let mut tail = server.open_sse_tail();

    server.sigint();
    let mut rest = Vec::new();
    tail.read_to_end(&mut rest)
        .expect("the sse stream ends on shutdown");
    let status = server.wait_for_exit();
    assert!(status.success(), "exit status: {status}");
    server.stderr_line_containing("stopping applets and syncs");
}

#[test]
fn sigint_exits_by_the_deadline_while_a_request_never_finishes() {
    let dir = root();
    let mut server = start(&dir.path().join("data"));
    let _stuck = server.open_stuck_request();

    server.sigint();
    let status = server.wait_for_exit();
    assert!(status.success(), "exit status: {status}");
    server.stderr_line_containing("shutdown still running");
}

#[test]
fn second_sigint_exits_at_once() {
    let dir = root();
    let mut server = start(&dir.path().join("data"));
    let _stuck = server.open_stuck_request();

    server.sigint();
    // The second handler is armed only once the first signal was seen.
    server.stderr_line_containing("Ctrl-C again to exit now");
    server.sigint();
    let status = server.wait_for_exit();
    assert_eq!(status.code(), Some(130), "exit status: {status}");
}

#[test]
fn sigint_exits_promptly_with_no_open_connection() {
    let dir = root();
    let mut server = start(&dir.path().join("data"));
    server.sigint();
    let status = server.wait_for_exit();
    assert!(status.success(), "exit status: {status}");
    server.stderr_line_containing("stopping applets");
}
