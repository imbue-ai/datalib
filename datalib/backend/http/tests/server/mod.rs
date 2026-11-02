//! `datalib-http` run as its own process, for the tests that need the
//! real binary: its signals, its working directory.

// Each test binary that includes this uses only part of it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const STARTUP: Duration = Duration::from_secs(30);
/// Well past `SHUTDOWN_DEADLINE` in `main.rs`; a loaded runner adds
/// seconds, a hang adds forever.
pub const EXIT: Duration = Duration::from_secs(30);

pub struct Server {
    child: Child,
    pub origin: String,
    pub token: String,
    /// Every stderr line so far; the log goes there and the test reads
    /// it for what the server said it was doing.
    stderr: mpsc::Receiver<String>,
}

/// The contents of `path` once it has some, or a failure naming what
/// never arrived rather than a bare timeout.
pub fn wait_for_file(path: &Path, what: &str) -> String {
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

pub fn start(root: &Path) -> Server {
    start_in(Path::new("."), root)
}

/// The server started from `cwd`, given `root` as it is: relative to
/// `cwd`, if relative.
pub fn start_in(cwd: &Path, root: &Path) -> Server {
    let abs_root = cwd.join(root);
    let url_file = abs_root.parent().unwrap().join("url");
    let mut child = Command::new(bin())
        .current_dir(cwd)
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
    let token = wait_for_file(&abs_root.join("system/api-token"), "api token")
        .trim()
        .to_string();
    // The signal handlers are registered before the url file is written,
    // so a SIGINT from here on is a shutdown, never a death by signal.
    Server {
        child,
        origin,
        token,
        stderr,
    }
}

/// Absolute, so a test may start the server from any directory.
fn bin() -> std::path::PathBuf {
    std::path::absolute(std::env::var("DATALIB_HTTP_BIN").expect("DATALIB_HTTP_BIN is set"))
        .expect("absolute DATALIB_HTTP_BIN")
}

impl Server {
    /// `body` POSTed to `path` as JSON; the response, head and all.
    pub fn post_json(&self, path: &str, body: &str) -> String {
        let host = self.origin.trim_start_matches("http://");
        let mut sock = TcpStream::connect(host).expect("connect");
        sock.set_read_timeout(Some(STARTUP)).unwrap();
        write!(
            sock,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.token,
            body.len()
        )
        .unwrap();
        let mut got = String::new();
        sock.read_to_string(&mut got).expect("read response");
        got
    }

    /// Holds `GET /api/sync/stream` open and returns once the first
    /// frame arrived, so the connection is established at the server
    /// too. The stream stays open for as long as the returned socket
    /// lives.
    pub fn open_sse_tail(&self) -> TcpStream {
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

    /// Starts a request whose body never arrives, and returns once the
    /// handler is waiting on it (hyper answers `100 Continue` when the
    /// body is first read), so the request is in flight at the server.
    /// It holds graceful shutdown open for as long as the socket lives.
    pub fn open_stuck_request(&self) -> TcpStream {
        let host = self.origin.trim_start_matches("http://");
        let mut sock = TcpStream::connect(host).expect("connect");
        sock.set_read_timeout(Some(STARTUP)).unwrap();
        write!(
            sock,
            "POST /api/config/check HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nExpect: 100-continue\r\n\r\n",
            self.token
        )
        .unwrap();
        let mut got = String::new();
        let mut buf = [0u8; 256];
        while !got.contains("\r\n\r\n") {
            let n = sock.read(&mut buf).expect("read 100 Continue");
            assert!(n > 0, "connection closed before 100 Continue:\n{got}");
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(got.starts_with("HTTP/1.1 100"), "response:\n{got}");
        sock
    }

    pub fn sigint(&self) {
        let status = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -INT");
    }

    pub fn stderr_line_containing(&self, needle: &str) -> String {
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

    pub fn wait_for_exit(&mut self) -> std::process::ExitStatus {
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
