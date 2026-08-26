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
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::frontend::frontend_dir;
use datalib_http::ApiToken;
use datalib_http::{router, AppState};
use datalib_unified_index::dolt_repo::DoltRepo;
use datalib_unified_index::qmd::{QmdDaemon, QmdDaemonConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "applet-test-token";

async fn state_with(root: &Path, config_toml: &str) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let dolt = DoltRepo::open(root.clone()).await.unwrap();
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let cfg = datalib_dag::config::parse(config_toml).expect("fixture config parses");
    datalib_dag::config::validate_applets(&cfg).expect("fixture config is valid");
    AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        app: Arc::new(app),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        // Every route is behind the per-process token; these tests
        // send it on each request (see `get_json`).
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::build(cfg.applets, (*root).clone(), None)),
    }
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
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

/// The real applet host, built by Bazel and handed over in `env`,
/// spelled the way a config would: binary plus subcommand.
///
/// Using it rather than only a fixture is the point of this file: it is
/// the one test where an applet's hand-written HTTP responses and the
/// gateway's hand-written parser actually meet.
fn applet_command() -> String {
    let bin = PathBuf::from(std::env::var("APPLET_BIN").expect("APPLET_BIN set by the BUILD rule"))
        .canonicalize()
        .expect("applet binary exists");
    format!("{} slack", bin.display())
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
              {{"channel":"{channel}","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"md1","message_index":0,"author":"ann","text":"first thread"}},
              {{"channel":"{channel}","when_ts":"2026-01-01T00:01:00Z","markdown_uuid":"md1","message_index":1,"author":"bob","text":"a reply"}}
            ]}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        tree.join("md2.grid_rows.json"),
        format!(
            r#"{{"rows":[
              {{"channel":"{channel}","when_ts":"2026-02-01T00:00:00Z","markdown_uuid":"md2","message_index":null,"text":"second thread"}},
              {{"channel":"{channel}","when_ts":"2026-02-01T00:00:00Z","markdown_uuid":"md2","message_index":0,"author":"cid","text":"second thread"}}
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
        bin = applet_command()
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    // The write ran without starting a server, and the namespace it
    // produced carries its own id as the component's argument.
    let (_, view) = get_json(&app, "/api/frontend").await;
    assert!(view["applet_errors"].is_null(), "{view}");
    let entry = &view["namespaces"]["slack_work"]["entries"]["channels"];
    assert_eq!(entry["component_args"], serde_json::json!(["slack_work"]));

    // Now the data call, which is what starts the process.
    let (status, body) = get_json(&app, "/applet/slack_work/channels").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspace"], "Work");
    assert_eq!(body["channels"][0]["name"], "#eng");
    // Two documents, three messages between them. Counting threads is
    // the regression guard: an earlier version collapsed a channel to a
    // single document, which made a busy channel look like one message.
    assert_eq!(body["channels"][0]["threads"], 2);
    assert_eq!(body["channels"][0]["messages"], 3);

    // Level 2 lists the channel's threads, each showing its opening
    // message with the rest counted as replies — the shape the Slack
    // app has.
    let (status, body) = get_json(&app, "/applet/slack_work/channel?name=%23eng").await;
    assert_eq!(status, StatusCode::OK);
    let threads = body["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    // Oldest first, the way a channel reads.
    assert_eq!(threads[0]["markdown_uuid"], "md1");
    assert_eq!(threads[0]["author"], "ann");
    assert_eq!(threads[0]["text"], "first thread");
    assert_eq!(
        threads[0]["replies"], 1,
        "everything after the opening message"
    );
    // A thread whose opening message is all there is has nothing to
    // expand, which is what the card keys its link on.
    assert_eq!(threads[1]["replies"], 0);

    // The channel name carries a '#', which a URL would read as a
    // fragment — so the encoding has to survive the proxy hop.
    let (_, body) = get_json(&app, "/applet/slack_work/channel?name=%23nope").await;
    assert!(body["threads"].as_array().unwrap().is_empty());

    // A 404 from the applet survives the proxy as a 404, not a 502.
    let (status, body) = get_json(&app, "/applet/slack_work/nope").await;
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
    let bin = applet_command();
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
        bin = bin
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    let (_, view) = get_json(&app, "/api/frontend").await;
    let a = &view["namespaces"]["a"]["entries"]["channels"];
    let b = &view["namespaces"]["b"]["entries"]["channels"];
    // Same component, different bound argument.
    assert_eq!(a["component_hash"], b["component_hash"]);
    assert_eq!(a["component_args"], serde_json::json!(["a"]));
    assert_eq!(b["component_args"], serde_json::json!(["b"]));

    // Each namespace holds its own copy on disk; they share an
    // *address*, which is what makes them one URL in the browser.
    let hash = a["component_hash"].as_str().unwrap();
    for ns in ["a", "b"] {
        assert!(frontend_dir(tmp.path())
            .join(ns)
            .join(format!("{hash}.js"))
            .is_file());
    }

    let (_, a_body) = get_json(&app, "/applet/a/channels").await;
    let (_, b_body) = get_json(&app, "/applet/b/channels").await;
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
