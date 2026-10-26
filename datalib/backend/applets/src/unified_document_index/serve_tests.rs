//! The `unified_document_index` applet as the gateway runs it: the real binary, on
//! port 0, with the data root and the secret in its environment and the
//! parent pipe on stdin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SECRET: &str = "a-test-secret";
const DEADLINE: Duration = Duration::from_secs(30);

fn applet_bin() -> Command {
    // Absolute, because the gateway starts the applet in the data root.
    let bin = std::env::var("APPLET_BIN").expect("APPLET_BIN from the BUILD rule");
    let mut cmd = Command::new(std::fs::canonicalize(bin).expect("the applet binary"));
    cmd.env_remove(super::DATA_ROOT_ENV);
    cmd
}

fn applet() -> Command {
    let mut cmd = applet_bin();
    cmd.args(["unified_document_index", "-p", "0"])
        .env(crate::gate::ENV_SECRET, SECRET);
    cmd
}

fn search_command(root: &std::path::Path, q: &str) -> Command {
    let mut cmd = applet_bin();
    cmd.arg("search").arg(root).arg(q);
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

fn post(port: u16, path: &str, body: &str) -> (u16, serde_json::Value) {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(
        conn,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{}: {SECRET}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        crate::gate::SECRET_HEADER,
        body.len()
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

/// Who a handle is, from the index alone: every source that mentioned
/// Picard's address gives one account of him, and a handle no source
/// mentions is absent rather than an error.
#[test]
fn people_by_handle_gives_each_sources_account() {
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::copy_grid_index(root.path());
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);

    let picard = "email:picard@enterprise.starfleet";
    let (status, body) = post(
        port,
        "/people",
        &format!(r#"{{"handles":["{picard}","email:nobody@nowhere.test"]}}"#),
    );
    assert_eq!(status, 200, "{body}");
    let people = body["people"].as_object().expect("a map by handle");
    assert_eq!(people.len(), 1, "{body}");
    let mut sources: Vec<&str> = people[picard]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["source_id"].as_str().unwrap())
        .collect();
    sources.sort();
    assert_eq!(
        sources,
        [
            "google-drive",
            "google-takeout",
            "slack",
            "tng_contacts",
            "tng_email"
        ],
        "{body}"
    );
    for c in people[picard].as_array().unwrap() {
        assert_eq!(c["names"][0], "Jean-Luc Picard", "{body}");
    }
    child.kill().ok();
}

fn percent_encoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `datalib-applet search` prints what `/search` answers, for the same
/// root and the same parameters, byte for byte as JSON: structured terms,
/// paging and sort, an identifier, the Fields and Words tabs, and a
/// refusal. It exits 1 on the refusal, saying why on stderr.
#[test]
fn the_search_command_answers_what_the_search_box_gets() {
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::materialize_root_with_grid(root.path());
    let mut server = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut server);

    let searches: &[(&str, &[(&str, &str)])] = &[
        ("", &[("limit", "3")]),
        (
            "is:document",
            &[("limit", "2"), ("offset", "2"), ("sort", "created_at:asc")],
        ),
        ("picard@enterprise.starfleet", &[("limit", "5")]),
        ("enterprise", &[("tab", "fields"), ("limit", "5")]),
        ("enterprise", &[("tab", "words"), ("limit", "5")]),
        ("frm:picard", &[]),
    ];
    for (q, params) in searches {
        let mut path = format!("/search?q={}", percent_encoded(q));
        let mut cli = search_command(root.path(), q);
        for (key, value) in *params {
            path.push_str(&format!("&{key}={}", percent_encoded(value)));
            cli.arg(format!("--{key}")).arg(value);
        }
        let (status, served) = get(port, &path, Some(SECRET));
        assert_eq!(status, 200, "{served}");
        let out = cli.output().expect("run the search command");
        let printed: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("the search command prints JSON");
        assert_eq!(printed, served, "{q:?} {params:?}");
        let refused = !served["refused"].as_array().is_none_or(Vec::is_empty);
        let rows = served["rows"].as_array().map_or(0, Vec::len);
        assert!(
            refused || rows > 0,
            "{q:?} found nothing, so it compares nothing"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(if refused { 1 } else { 0 }),
            "{stderr}"
        );
        if refused {
            assert!(stderr.contains("`frm:`"), "{stderr}");
        }
    }
    server.kill().ok();
}

/// A path that is not a data root is an error naming what is missing, not
/// an empty answer an agent would read as "nothing matched".
#[test]
fn the_search_command_refuses_a_directory_with_no_index() {
    let empty = tempfile::tempdir().unwrap();
    let out = search_command(empty.path(), "")
        .output()
        .expect("run the search command");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("has no grid index"), "{stderr}");
}
