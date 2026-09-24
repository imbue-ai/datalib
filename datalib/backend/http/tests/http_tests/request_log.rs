//! Every request the app makes leaves a line in the server's log —
//! `system/runs/runs.sqlite`, read back through `GET /api/log` — except the
//! reads of the log itself, which would otherwise wake the log panel
//! into refetching forever. One test, because the subscriber it
//! installs is the process's only one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "request-log-test-token";
const CARD: &str = "0192f6a0-0000-7000-8000-000000000000";

async fn state(root: &Path) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        sync: datalib_http::supervisor::SyncControl::new(root.clone()),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        usage: Default::default(),
        newer_root: Vec::new(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn send(root: &Path, uri: &str, with_token: bool) -> (StatusCode, Vec<u8>) {
    send_with(root, uri, with_token, &[]).await
}

async fn send_with(
    root: &Path,
    uri: &str,
    with_token: bool,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<u8>) {
    let app = router(state(root).await);
    let mut req = Request::builder().uri(uri);
    if with_token {
        req = req.header("x-datalib-token", TEST_TOKEN);
    }
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// The request lines in the store, oldest first.
async fn request_lines(root: &Path) -> Vec<serde_json::Value> {
    let (status, body) = send(root, "/api/log", true).await;
    assert_eq!(status, StatusCode::OK);
    let lines: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    lines
        .into_iter()
        .filter(|l| l["target"] == datalib_http::request_log::TARGET)
        .collect()
}

fn fields(line: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(line["fields"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn every_request_but_a_read_of_the_log_leaves_a_line() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let log = datalib_http::logging::init(root).expect("the store opens in a temp dir");

    // A plain API read, with the token in the header.
    let (status, _) = send(root, "/api/health", true).await;
    assert_eq!(status, StatusCode::OK);
    // The token on the query string is dropped from the line; the rest
    // stays. Made by a card, which names itself.
    let (status, _) = send_with(
        root,
        &format!("/api/health?token={TEST_TOKEN}&x=1"),
        false,
        &[
            ("x-datalib-card", CARD),
            ("x-datalib-card-type", "gridView"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A refused request is a line too: the gate sits inside the log layer.
    let (status, _) = send(root, "/api/config", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A cached asset that failed is worth a line; one that was served is not.
    let (status, _) = send(root, "/modules/not-a-hash", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(root, "/dactal/index.html", false).await;
    assert_eq!(status, StatusCode::OK);
    // The read the log panel makes on every `log` frame. Made while the
    // writer is still up, so a line for it would be flushed with the rest.
    let (status, _) = send(root, "/api/log", true).await;
    assert_eq!(status, StatusCode::OK);

    // The writer flushes on an interval; dropping it is the final flush.
    drop(log);

    let lines = request_lines(root).await;
    let paths: Vec<&str> = lines
        .iter()
        .map(|l| fields(l)["path"].as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        paths,
        [
            "/api/health",
            "/api/health",
            "/api/config",
            "/modules/not-a-hash"
        ],
        "{lines:#?}"
    );

    let health = fields(&lines[0]);
    assert_eq!(health["method"], "GET");
    assert_eq!(health["status"], 200);
    assert!(health["ms"].is_u64(), "{health}");
    assert!(health.get("query").is_none(), "{health}");
    assert!(health.get("card").is_none(), "{health}");
    assert_eq!(lines[0]["level"], "info");
    assert_eq!(lines[0]["process"], "http");
    assert!(lines[0]["msg"]
        .as_str()
        .unwrap()
        .starts_with("GET /api/health 200 "));

    let with_query = fields(&lines[1]);
    assert_eq!(with_query["query"], "x=1");
    assert_eq!(with_query["card"], CARD);
    assert_eq!(with_query["card_type"], "gridView");

    let refused = fields(&lines[2]);
    assert_eq!(refused["status"], 401);

    let missing = fields(&lines[3]);
    assert_eq!(missing["status"], 404);
}
