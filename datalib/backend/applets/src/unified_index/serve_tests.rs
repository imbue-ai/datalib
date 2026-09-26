//! The `unified_index` applet as the gateway runs it: the real binary, on
//! port 0, with the data root and the secret in its environment and the
//! parent pipe on stdin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SECRET: &str = "a-test-secret";
const DEADLINE: Duration = Duration::from_secs(30);

fn applet() -> Command {
    // Absolute, because the gateway starts the applet in the data root.
    let bin = std::env::var("APPLET_BIN").expect("APPLET_BIN from the BUILD rule");
    let mut cmd = Command::new(std::fs::canonicalize(bin).expect("the applet binary"));
    cmd.args(["unified_index", "-p", "0"])
        .env(crate::gate::ENV_SECRET, SECRET)
        .env_remove(super::DATA_ROOT_ENV);
    cmd
}

/// The port the applet announces on stdout, once it is listening.
fn announced_port(child: &mut Child) -> u16 {
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(port) = line.strip_prefix("DATALIB_APPLET_PORT=") {
                let _ = tx.send(port.parse::<u16>().expect("a port number"));
            }
        }
    });
    rx.recv_timeout(DEADLINE)
        .expect("the applet never announced a port")
}

fn get(port: u16, path: &str, secret: Option<&str>) -> (u16, serde_json::Value) {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let secret = secret
        .map(|s| format!("{}: {s}\r\n", crate::gate::SECRET_HEADER))
        .unwrap_or_default();
    write!(
        conn,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{secret}Connection: close\r\n\r\n"
    )
    .expect("send");
    let mut response = String::new();
    conn.read_to_string(&mut response).expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let status = head[9..12].parse().expect("a status code");
    (status, serde_json::from_str(body).expect("a JSON body"))
}

fn exit_code_within_deadline(child: &mut Child) -> Option<i32> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status.code();
        }
        assert!(start.elapsed() < DEADLINE, "the applet did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Started the way the gateway starts it, the applet announces the port it
/// bound, answers a search that carries the secret, refuses one that does
/// not, and exits cleanly when its parent's pipe closes.
#[test]
fn it_serves_the_root_it_was_started_on_and_leaves_with_its_parent() {
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::copy_grid_index(root.path());
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .env("DATALIB_PARENT_PIPE", "0")
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);

    let (status, body) = get(port, "/search?q=&limit=3", Some(SECRET));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"].as_array().map(Vec::len), Some(3), "{body}");
    assert!(body["total"].as_u64() > Some(3), "{body}");

    let (status, _) = get(port, "/search?q=", None);
    assert_eq!(status, 401, "a request without the secret got through");

    drop(child.stdin.take());
    assert_eq!(exit_code_within_deadline(&mut child), Some(0));
}

/// With no data root the applet says which variable it wanted and stops,
/// rather than serving whatever directory it happened to start in.
#[test]
fn without_a_data_root_it_says_so_and_stops() {
    let out = applet().output().expect("run the applet");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(super::DATA_ROOT_ENV), "{stderr}");
}
