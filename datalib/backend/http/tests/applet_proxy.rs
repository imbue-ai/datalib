//! Integration test for the applet **proxy**: a spawned server, a real
//! loopback socket, and the gateway's hand-written HTTP/1.1 client
//! parsing bytes a separate hand-written server produced.
//!
//! Split out of `applet_endpoint.rs` because it binds ports and runs
//! programs from the ambient environment, so it carries
//! `no-sandbox`/`requires-network` (the same treatment the UI e2e test
//! takes). Everything that can be hermetic stays in the other file.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::dolt_repo::DoltRepo;
use datalib_core::qmd::{QmdDaemon, QmdDaemonConfig};
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, AppState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

async fn state_with(root: &Path, config_toml: &str) -> AppState {
    let db_path = root.join("backend_index.doltlite_db");
    let root = Arc::new(root.to_path_buf());
    let dolt = DoltRepo::open(&db_path, root.clone()).await.unwrap();
    let cfg = datalib_dag::config::parse(config_toml).expect("fixture config parses");
    datalib_dag::config::validate_applets(&cfg).expect("fixture config is valid");
    AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        applets: Arc::new(AppletRegistry::discover(cfg.applets, (*root).clone(), None)),
    }
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// The real reference applet, built by Bazel and handed over in `env`.
/// Using it rather than only a fixture is the point of this file: it
/// is the one test where the applet's hand-written HTTP responses and
/// the gateway's hand-written parser actually meet.
fn slack_applet_bin() -> PathBuf {
    PathBuf::from(
        std::env::var("SLACK_APPLET_BIN").expect("SLACK_APPLET_BIN set by the BUILD rule"),
    )
    .canonicalize()
    .expect("applet binary exists")
}

/// A rendered tree shaped the way Slack renders: one document per
/// thread, with the thread row and its messages all carrying that
/// document's `markdown_uuid`.
fn seed_tree(root: &Path, rel: &str, channel: &str) {
    let tree = root.join(rel);
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(
        tree.join("md1.grid_rows.json"),
        format!(
            r#"{{"rows":[
              {{"channel":"{channel}","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"md1","message_index":null,"text":"first thread"}},
              {{"channel":"{channel}","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"md1","message_index":0,"text":"first thread"}},
              {{"channel":"{channel}","when_ts":"2026-01-01T00:01:00Z","markdown_uuid":"md1","message_index":1,"text":"a reply"}}
            ]}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        tree.join("md2.grid_rows.json"),
        format!(
            r#"{{"rows":[
              {{"channel":"{channel}","when_ts":"2026-02-01T00:00:00Z","markdown_uuid":"md2","message_index":null,"text":"second thread"}},
              {{"channel":"{channel}","when_ts":"2026-02-01T00:00:00Z","markdown_uuid":"md2","message_index":0,"text":"second thread"}}
            ]}}"#
        ),
    )
    .unwrap();
}

/// End to end against the shipped applet: discovery, module store,
/// lazy spawn, proxy, and the applet's own response parsing.
#[tokio::test]
async fn the_reference_applet_round_trips_through_the_gateway() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path(), "slack/rendered_md", "#eng");
    let cfg = format!(
        r#"
[[applets]]
id = "slack_work"
title = "Work Slack"
command = "{bin}"
[applets.params]
tree = "slack/rendered_md"
workspace = "Work"
"#,
        bin = slack_applet_bin().display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    // Discovery ran without starting a server.
    let (_, applets) = get_json(&app, "/api/applets").await;
    let a = &applets.as_array().unwrap()[0];
    assert!(a.get("error").is_none(), "{a}");
    assert_eq!(
        a["gallery"][0]["source"],
        r#"slack_work.channels("slack_work")"#
    );

    // Now the data call, which is what starts the process.
    let (status, body) = get_json(&app, "/v/slack_work/channels").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspace"], "Work");
    assert_eq!(body["channels"][0]["name"], "#eng");
    // Two documents, three messages between them. Counting threads is
    // the regression guard: an earlier version collapsed a channel to a
    // single document, which made a busy channel look like one message.
    assert_eq!(body["channels"][0]["threads"], 2);
    assert_eq!(body["channels"][0]["messages"], 3);

    // Drilling in lists the channel's threads, newest first, each with
    // the document a click should open.
    let (status, body) = get_json(&app, "/v/slack_work/threads?channel=%23eng").await;
    assert_eq!(status, StatusCode::OK);
    let threads = body["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0]["markdown_uuid"], "md2");
    assert_eq!(threads[0]["title"], "second thread");
    assert_eq!(threads[1]["markdown_uuid"], "md1");
    assert_eq!(threads[1]["messages"], 2);

    // The channel name carries a '#', which a URL would read as a
    // fragment — so the encoding has to survive the proxy hop.
    let (_, body) = get_json(&app, "/v/slack_work/threads?channel=%23nope").await;
    assert!(body["threads"].as_array().unwrap().is_empty());

    // A 404 from the applet survives the proxy as a 404, not a 502.
    let (status, body) = get_json(&app, "/v/slack_work/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("/nope"), "{body}");
}

/// Two instances of the reference binary over two trees: one module in
/// the store, two independently-served datasets.
#[tokio::test]
async fn two_reference_instances_share_a_module_and_serve_their_own_data() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path(), "work/rendered_md", "#eng");
    seed_tree(tmp.path(), "home/rendered_md", "#family");
    let bin = slack_applet_bin();
    let cfg = format!(
        r#"
[[applets]]
id = "a"
command = "{bin}"
[applets.params]
tree = "work/rendered_md"
workspace = "Work"

[[applets]]
id = "b"
command = "{bin}"
[applets.params]
tree = "home/rendered_md"
workspace = "Home"
"#,
        bin = bin.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    let (_, applets) = get_json(&app, "/api/applets").await;
    let list = applets.as_array().unwrap();
    assert_eq!(
        list[0]["components"]["channels"],
        list[1]["components"]["channels"]
    );
    assert_eq!(list[0]["gallery"][0]["source"], r#"a.channels("a")"#);
    assert_eq!(list[1]["gallery"][0]["source"], r#"b.channels("b")"#);

    let store: Vec<_> = std::fs::read_dir(datalib_http::applets::module_store_dir(tmp.path()))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != "CACHEDIR.TAG")
        .collect();
    assert_eq!(store.len(), 1, "expected one shared module");

    let (_, a_body) = get_json(&app, "/v/a/channels").await;
    let (_, b_body) = get_json(&app, "/v/b/channels").await;
    assert_eq!(a_body["channels"][0]["name"], "#eng");
    assert_eq!(b_body["channels"][0]["name"], "#family");
}

// The client half on its own, against a listener this test owns.
//
// The reference-applet tests above prove the round trip works; these
// pin the wire format for shapes that applet does not produce — an
// arbitrary status code, and a request body whose content type is not
// JSON. Driving `forward` directly is what lets the request bytes
// themselves be asserted.

use std::io::{Read, Write};
use std::net::TcpListener;

/// Bind a listener, hand one canned response to the first connection,
/// and return what the client sent.
///
/// The request must be drained completely before responding: the
/// client does not half-close its write side, so a server that closes
/// with bytes still unread makes the kernel answer with RST and the
/// client sees "connection reset" instead of the response.
fn one_shot_server(response: &'static [u8]) -> (u16, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        // Read until the head is complete, then exactly as many body
        // bytes as it declared.
        let head_end = loop {
            if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = sock.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break raw.len();
            }
            raw.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
        let want: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        while raw.len() - head_end < want {
            let n = sock.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        let seen = String::from_utf8_lossy(&raw).to_string();
        sock.write_all(response).unwrap();
        sock.flush().unwrap();
        drop(sock);
        seen
    });
    (port, handle)
}

#[tokio::test]
async fn forwards_the_callers_content_type_verbatim() {
    let (port, seen) =
        one_shot_server(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}");
    let resp = tokio::task::spawn_blocking(move || {
        datalib_http::applets::forward(port, "POST", "/thing?a=1", Some("text/csv"), b"a,b\n1,2")
    })
    .await
    .unwrap()
    .expect("forward succeeds");

    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "application/json");
    assert_eq!(resp.body, b"{\"ok\":true}");

    let req = seen.join().unwrap();
    assert!(req.starts_with("POST /thing?a=1 HTTP/1.1\r\n"), "{req:?}");
    // The caller's type, not an invented one — this is the bug the
    // hardcoded `application/json` used to be.
    assert!(req.contains("Content-Type: text/csv\r\n"), "{req:?}");
    assert!(req.contains("Content-Length: 7\r\n"), "{req:?}");
    assert!(req.ends_with("a,b\n1,2"), "{req:?}");
}

#[tokio::test]
async fn preserves_an_arbitrary_status_and_type() {
    let (port, seen) = one_shot_server(
        b"HTTP/1.1 418 I'm a teapot\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nshort and stout",
    );
    let resp = tokio::task::spawn_blocking(move || {
        datalib_http::applets::forward(port, "GET", "/teapot", None, b"")
    })
    .await
    .unwrap()
    .expect("forward succeeds");
    assert_eq!(resp.status, 418);
    assert_eq!(resp.content_type, "text/plain; charset=utf-8");
    assert_eq!(resp.body, b"short and stout");

    let req = seen.join().unwrap();
    // A bodyless request declares neither length nor type.
    assert!(!req.contains("Content-Length"), "{req:?}");
    assert!(!req.contains("Content-Type"), "{req:?}");
}
