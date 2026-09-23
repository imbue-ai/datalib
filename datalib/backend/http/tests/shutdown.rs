//! Ctrl-C ends `datalib-http` even while a browser tab holds
//! `/api/sync/stream` open. Graceful shutdown waits for every connection,
//! and that SSE tail never ends on its own, so without a deadline the
//! first Ctrl-C closed the listener and the process sat there for good.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const STARTUP: Duration = Duration::from_secs(30);
/// Well past `SHUTDOWN_DEADLINE` in `main.rs`; a loaded runner adds
/// seconds, a hang adds forever.
const EXIT: Duration = Duration::from_secs(30);

struct Server {
    child: Child,
    origin: String,
    token: String,
    /// Every stderr line so far; the log goes there and the test reads
    /// it for what the server said it was doing.
    stderr: mpsc::Receiver<String>,
}

/// The contents of `path` once it has some, or a failure naming what
/// never arrived rather than a bare timeout.
fn wait_for_file(path: &Path, what: &str) -> String {
    let deadline = Instant::now() + STARTUP;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "datalib-http never wrote its {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start(root: &Path) -> Server {
    let url_file = root.parent().unwrap().join("url");
    let mut child =
        Command::new(std::env::var("DATALIB_HTTP_BIN").expect("DATALIB_HTTP_BIN is set"))
            .arg("--no-open")
            .arg("--url-file")
            .arg(&url_file)
            .arg(root)
            .env("DATALIB_BIND", "127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn datalib-http");
    let (tx, stderr) = mpsc::channel();
    let pipe = child.stderr.take().expect("stderr");
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let url = wait_for_file(&url_file, "url file");
    let origin = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?']).next())
        .map(|host| format!("http://{host}"))
        .expect("url has a host");
    // The url file is written before `build_state`, and `build_state` is
    // what writes the token file — so the url arriving says nothing
    // about the token. Under load the gap is wide enough to lose.
    let token = wait_for_file(&root.join("system/api-token"), "api token")
        .trim()
        .to_string();
    let server = Server {
        child,
        origin,
        token,
        stderr,
    };
    // The url file lands before `axum::serve` — and with it the signal
    // handler — is up. Serving a request is what says both are.
    server.request("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    server
}

impl Server {
    fn request(&self, raw: &str) -> String {
        let host = self.origin.trim_start_matches("http://");
        let mut sock = TcpStream::connect(host).expect("connect");
        sock.set_read_timeout(Some(STARTUP)).unwrap();
        sock.write_all(raw.as_bytes()).unwrap();
        let mut got = Vec::new();
        sock.read_to_end(&mut got).expect("read response");
        String::from_utf8_lossy(&got).into_owned()
    }

    /// Holds `GET /api/sync/stream` open and returns once the first
    /// frame arrived, so the connection is established at the server
    /// too. The stream stays open for as long as the returned socket
    /// lives.
    fn open_sse_tail(&self) -> TcpStream {
        let host = self.origin.trim_start_matches("http://");
        let mut sock = TcpStream::connect(host).expect("connect");
        sock.set_read_timeout(Some(STARTUP)).unwrap();
        write!(
            sock,
            "GET /api/sync/stream HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nAccept: text/event-stream\r\n\r\n",
            self.token
        )
        .unwrap();
        let mut got = String::new();
        let mut buf = [0u8; 1024];
        while !got.contains("\r\n\r\n") || !got.contains("\n\n") {
            let n = sock.read(&mut buf).expect("read sse");
            assert!(n > 0, "sse stream closed before its first frame:\n{got}");
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(got.starts_with("HTTP/1.1 200"), "sse response:\n{got}");
        sock
    }

    fn sigint(&self) {
        let status = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -INT");
    }

    fn stderr_line_containing(&self, needle: &str) -> String {
        let deadline = Instant::now() + EXIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.stderr.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return line,
                Ok(_) => continue,
                Err(_) => panic!("datalib-http never said {needle:?} on stderr"),
            }
        }
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + EXIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("datalib-http still running {EXIT:?} after SIGINT");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn root() -> tempfile::TempDir {
    tempfile::TempDir::with_prefix("datalib-http-shutdown-itest-").expect("tempdir")
}

#[test]
fn sigint_exits_while_an_sse_tail_is_open() {
    let dir = root();
    let mut server = start(&dir.path().join("data"));
    let _tail = server.open_sse_tail();

    server.sigint();
    let status = server.wait_for_exit();
    assert!(status.success(), "exit status: {status}");
    server.stderr_line_containing("shutdown still running");
}

#[test]
fn second_sigint_exits_at_once() {
    let dir = root();
    let mut server = start(&dir.path().join("data"));
    let _tail = server.open_sse_tail();

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
