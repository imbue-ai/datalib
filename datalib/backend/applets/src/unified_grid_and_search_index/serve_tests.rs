//! The `unified_grid_and_search_index` applet as the gateway runs it: the real binary, on
//! port 0, with the data root and the secret's file in its environment and
//! the parent pipe on stdin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, OnceLock};
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

fn secret_file() -> &'static Path {
    static FILE: OnceLock<tempfile::TempPath> = OnceLock::new();
    FILE.get_or_init(|| {
        let file = tempfile::NamedTempFile::new()
            .expect("temp file")
            .into_temp_path();
        datalib_env::secret_file::write(&file, SECRET).expect("write the secret");
        file
    })
}

fn applet() -> Command {
    let mut cmd = applet_bin();
    cmd.args(["unified_grid_and_search_index", "-p", "0"])
        .env(crate::gate::ENV_SECRET_FILE, secret_file());
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
    datalib_search_fixture::copy_grid_index(root.path());
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

/// An applet that has loaded the search model still exits cleanly when its
/// parent goes, as when the app quits. On a mac, ggml's Metal teardown
/// aborted the process while the model was loaded, leaving a crash report.
#[test]
fn it_leaves_with_its_parent_after_a_search_by_meaning() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::materialize_root(root.path());
    let log = root.path().join("applet.stderr");
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .env("DATALIB_PARENT_PIPE", "0")
        .env("RUST_LOG", "info")
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);

    let (status, body) = get(port, "/search?q=the%20enterprise&limit=3", Some(SECRET));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["query_echo"]["search_error"],
        serde_json::Value::Null,
        "{body}"
    );
    let stderr = std::fs::read_to_string(&log).unwrap();
    assert!(
        stderr.contains("loaded the search model"),
        "the search never loaded the model, so this exit tests nothing:\n{stderr}"
    );

    drop(child.stdin.take());
    let code = exit_code_within_deadline(&mut child);
    let stderr = std::fs::read_to_string(&log).unwrap();
    assert_eq!(code, Some(0), "{stderr}");
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
    datalib_search_fixture::copy_grid_index(root.path());
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
            "facebook",
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

/// `/sql` as the gateway reaches it: the store listing names the grid
/// index, a query over it answers a page, and neither route answers
/// without the secret.
#[test]
fn sql_reads_a_store_it_lists() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(root.path());
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);
    let grid = "unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite";

    let (status, body) = get(port, "/sql/stores", Some(SECRET));
    assert_eq!(status, 200, "{body}");
    let listed: Vec<&str> = body["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["store"].as_str().unwrap())
        .collect();
    assert!(listed.contains(&grid), "{body}");

    let (status, body) = post(
        port,
        "/sql",
        &serde_json::json!({
            "store": grid,
            "sql": "SELECT source_id, count(*) AS n FROM grid_rows WHERE source_id = :source GROUP BY 1",
            "params": {"source": "slack"},
        })
        .to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["columns"],
        serde_json::json!(["source_id", "n"]),
        "{body}"
    );
    assert_eq!(body["rows"][0][0], "slack", "{body}");
    assert!(body["rows"][0][1].as_i64() > Some(0), "{body}");
    assert_eq!(body["next_offset"], serde_json::Value::Null, "{body}");

    let (status, _) = get(port, "/sql/stores", None);
    assert_eq!(status, 401, "a request without the secret got through");
    child.kill().ok();
}

/// `/children` over the TNG fixture's index: a Slack channel's threads,
/// paged on from the start to the end in order, a window around one of
/// them and around now, and a refusal for an anchor elsewhere or a cursor
/// it never gave.
#[test]
fn children_page_through_a_channel() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(root.path());
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        // The fixture is dated in the 2360s.
        .env(super::SEARCH_NOW_ENV, "2400-01-01T00:00:00Z")
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);
    let (status, body) = post(
        port,
        "/sql",
        &serde_json::json!({
            "store": "unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite",
            "sql": "SELECT DISTINCT uuid FROM data_nodes WHERE kind = 'Slack Channel' AND name = '#bridge'",
        })
        .to_string(),
    );
    assert_eq!(status, 200, "{body}");
    let bridge = body["rows"][0][0].as_str().expect("#bridge").to_string();

    let mut threads: Vec<serde_json::Value> = Vec::new();
    let mut path = format!("/children/{bridge}?limit=2");
    loop {
        let (status, body) = get(port, &path, Some(SECRET));
        assert_eq!(status, 200, "{body}");
        threads.extend(body["children"].as_array().unwrap().iter().cloned());
        let Some(after) = body["after"].as_str() else {
            break;
        };
        path = format!("/children/{bridge}?after={after}&limit=2");
    }
    let keys: Vec<&str> = threads
        .iter()
        .map(|t| t["sort_key"].as_str().unwrap())
        .collect();
    assert!(threads.len() > 2, "{threads:?}");
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
    for t in &threads {
        assert_eq!(t["container"], true, "{t}");
        assert_eq!(t["kind"], "Slack Thread", "{t}");
        assert!(
            t["link"]
                .as_str()
                .is_some_and(|l| l.starts_with("datalib:doc/")),
            "{t}"
        );
    }

    let third = threads[2]["uuid"].as_str().unwrap();
    let (status, body) = get(
        port,
        &format!("/children/{bridge}?at={third}&limit=1"),
        Some(SECRET),
    );
    assert_eq!(status, 200, "{body}");
    let around: Vec<&serde_json::Value> = body["children"].as_array().unwrap().iter().collect();
    assert_eq!(around, threads[1..4].iter().collect::<Vec<_>>(), "{body}");

    let (status, body) = get(
        port,
        &format!("/children/{bridge}?at=now&limit=1"),
        Some(SECRET),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["children"][0], threads[threads.len() - 1], "{body}");
    assert_eq!(body["after"], serde_json::Value::Null, "{body}");

    let (status, body) = get(
        port,
        &format!("/children/{third}?at={bridge}"),
        Some(SECRET),
    );
    assert_eq!(status, 404, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("is not a child of"),
        "{body}"
    );
    let (status, body) = get(port, &format!("/children/{bridge}?after=40"), Some(SECRET));
    assert_eq!(status, 400, "{body}");

    // A thread's messages name their authors by handle, which a person
    // chip is drawn from.
    let first = threads[0]["uuid"].as_str().unwrap();
    let (status, body) = get(port, &format!("/children/{first}"), Some(SECRET));
    assert_eq!(status, 200, "{body}");
    let messages = body["children"].as_array().unwrap();
    assert!(!messages.is_empty(), "{body}");
    for m in messages {
        let handle = m["author_handle"].as_str().unwrap_or_default();
        assert!(handle.starts_with("slack:"), "{m}");
    }
    child.kill().ok();
}

/// A library's tree over the TNG fixture, a level at a time: the email
/// source's top is its account, a place with places below it, and the
/// account's places are its labels, with no thread or message among them.
#[test]
fn children_list_a_sources_places() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(root.path());
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);

    let (status, body) = get(port, "/children/source:tng_email?places=true", Some(SECRET));
    assert_eq!(status, 200, "{body}");
    let top = body["children"].as_array().unwrap();
    assert_eq!(top.len(), 1, "{body}");
    assert_eq!(top[0]["kind"], "Email Account", "{body}");
    assert_eq!(top[0]["places_below"], true, "{body}");
    let account = top[0]["uuid"].as_str().unwrap();

    let (status, body) = get(
        port,
        &format!("/children/{account}?places=true&limit=1000"),
        Some(SECRET),
    );
    assert_eq!(status, 200, "{body}");
    let places = body["children"].as_array().unwrap();
    assert!(!places.is_empty(), "{body}");
    for p in places {
        assert_eq!(p["kind"], "Email Label", "{p}");
        assert_eq!(p["container"], true, "{p}");
        assert_eq!(p["places_below"], false, "{p}");
    }

    let (status, body) = get(port, &format!("/children/{account}"), Some(SECRET));
    assert_eq!(status, 200, "{body}");
    assert!(
        body["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "Email Thread"),
        "without places=true the account lists its threads too: {body}"
    );
    assert!(body["children"][0].get("places_below").is_none(), "{body}");
    child.kill().ok();
}

/// The TNG Drive opens onto its folders: My Drive, the shared drive and
/// a folder shared with the account at the top, each folder inside the
/// one above it, and each Doc in its folder, or filed under its second
/// one. Before, a Doc sat in a nameless folder of one level.
#[test]
fn a_drive_opens_onto_its_folders() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(root.path());
    let mut child = applet()
        .env(super::DATA_ROOT_ENV, root.path())
        .env(super::SEARCH_NOW_ENV, "2400-01-01T00:00:00Z")
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let port = announced_port(&mut child);
    let sql = |sql: &str| {
        let (status, body) = post(
            port,
            "/sql",
            &serde_json::json!({
                "store": "unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite",
                "sql": sql,
            })
            .to_string(),
        );
        assert_eq!(status, 200, "{body}");
        body["rows"].as_array().unwrap().clone()
    };
    let tops: Vec<(String, String)> = sql("SELECT DISTINCT kind, name FROM data_nodes \
         WHERE source_id = 'google-drive' AND is_place AND parent_uuid IS NULL ORDER BY name")
    .iter()
    .map(|r| (r[0].as_str().unwrap().into(), r[1].as_str().unwrap().into()))
    .collect();
    assert_eq!(
        tops,
        [
            ("Shared drive", "Enterprise-D Engineering"),
            ("Folder", "Geordi's notes"),
            ("Folder", "My Drive"),
        ]
        .map(|(k, n)| (k.to_string(), n.to_string()))
    );
    let place = |kind: &str, name: &str| {
        sql(&format!(
            "SELECT DISTINCT uuid FROM data_nodes WHERE source_id = 'google-drive' \
             AND kind = '{kind}' AND name = '{}'",
            name.replace('\'', "''")
        ))[0][0]
            .as_str()
            .unwrap()
            .to_string()
    };
    let doc = |title: &str| {
        sql(&format!(
            "SELECT uuid FROM grid_rows WHERE kind = 'Google Doc' AND conversation_name = '{}'",
            title.replace('\'', "''")
        ))[0][0]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Each child as (kind, its name for a folder or its uuid for a Doc).
    let children = |uuid: &str| {
        let (status, body) = get(port, &format!("/children/{uuid}"), Some(SECRET));
        assert_eq!(status, 200, "{body}");
        let mut out: Vec<(String, String)> = body["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let kind = c["kind"].as_str().unwrap().to_string();
                let said = match kind.as_str() {
                    "Google Doc" => c["uuid"].as_str(),
                    _ => c["name"].as_str(),
                };
                (kind, said.unwrap_or_default().to_string())
            })
            .collect();
        out.sort();
        out
    };
    let folder = |name: &str| ("Folder".to_string(), name.to_string());
    let a_doc = |title: &str| ("Google Doc".to_string(), doc(title));

    let my_drive = place("Folder", "My Drive");
    assert_eq!(
        children(&my_drive),
        vec![folder("Captain's logs"), folder("Ready room")]
    );
    assert_eq!(
        children(&place("Folder", "Captain's logs")),
        vec![folder("Stardate 41000s")]
    );
    let log = a_doc("Captain's log, stardate 41153.7");
    assert_eq!(
        children(&place("Folder", "Stardate 41000s")),
        vec![log.clone()]
    );
    assert_eq!(children(&place("Folder", "Ready room")), vec![log]);
    assert_eq!(
        children(&place("Shared drive", "Enterprise-D Engineering")),
        vec![folder("Engineering")]
    );
    assert_eq!(
        children(&place("Folder", "Engineering")),
        vec![folder("Warp core")]
    );
    assert_eq!(
        children(&place("Folder", "Warp core")),
        vec![a_doc("Warp core maintenance schedule")]
    );
    assert_eq!(
        children(&place("Folder", "Geordi's notes")),
        vec![a_doc("Engineering notes")]
    );
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
/// paging and sort, an address, words, a `tab` it no longer has, and a
/// refusal. It exits 1 on the refusal, saying why on stderr.
#[test]
fn the_search_command_answers_what_the_search_box_gets() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::materialize_root(root.path());
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
        ("enterprise semantic:off", &[("limit", "5")]),
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
            assert!(
                stderr.contains("`frm:`") || stderr.contains("`tab` is gone"),
                "{stderr}"
            );
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
