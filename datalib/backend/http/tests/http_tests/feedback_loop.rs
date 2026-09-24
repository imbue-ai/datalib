//! A page that refetches on every `log` frame feeds itself: its request
//! is a log line, the line is the next frame. The server counts the hops
//! (`loop_guard`) and warns once a chain is long enough to be a loop.
//! Played here with the real watcher, the real log writer and the real
//! router; the page is this test. One test, because the subscriber it
//! installs is the process's only one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::loop_guard::{CAUSE_HEADER, LOOP_AT, TARGET};
use datalib_http::watch::{RootEvent, RootFrame, Table};
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tower::ServiceExt;

const TEST_TOKEN: &str = "feedback-loop-test-token";

async fn state(root: &Path, root_tx: broadcast::Sender<RootFrame>) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        sync: datalib_http::supervisor::SyncControl::new(root.clone()),
        app: Arc::new(app),
        root_tx,
        usage: Default::default(),
        newer_root: Vec::new(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn get(state: &AppState, uri: &str, cause: Option<u32>) -> Vec<u8> {
    let mut req = Request::builder()
        .uri(uri)
        .header("x-datalib-token", TEST_TOKEN);
    if let Some(c) = cause {
        req = req.header(CAUSE_HEADER, c.to_string());
    }
    let resp = router(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec()
}

/// The next `log` frame, or `None` when none comes within a few
/// debounce windows.
async fn next_log_frame(rx: &mut broadcast::Receiver<RootFrame>) -> Option<RootFrame> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(f)) if f.event == (RootEvent::TableChanged { table: Table::Log }) => {
                return Some(f)
            }
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Plays a page that refetches on every `log` frame, for `hops` frames,
/// echoing each frame's chain when `echo` is set. The chains it saw.
async fn refetch_on_log_frames(
    state: &AppState,
    rx: &mut broadcast::Receiver<RootFrame>,
    hops: usize,
    echo: bool,
) -> Vec<Option<u32>> {
    // The first fetch is the page loading: nothing caused it.
    get(state, "/api/health", None).await;
    let mut seen = Vec::new();
    for _ in 0..hops {
        let frame = next_log_frame(rx)
            .await
            .expect("the request's own line should come back as a log frame");
        seen.push(frame.chain);
        get(state, "/api/health", frame.chain.filter(|_| echo)).await;
    }
    seen
}

async fn loop_warnings(state: &AppState) -> Vec<serde_json::Value> {
    let lines: Vec<serde_json::Value> =
        serde_json::from_slice(&get(state, "/api/log", None).await).unwrap();
    lines
        .into_iter()
        .filter(|l| l["target"] == TARGET)
        .collect()
}

/// Let every frame the last hop caused land, and drop them.
async fn settle(rx: &mut broadcast::Receiver<RootFrame>) {
    while next_log_frame(rx).await.is_some() {}
}

#[tokio::test]
async fn a_page_refetching_on_its_own_echo_is_warned_about_once_the_chain_is_a_loop() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let log = datalib_http::logging::init(root).expect("the store opens in a temp dir");
    let (tx, mut rx) = broadcast::channel(256);
    let state = state(root, tx.clone()).await;
    datalib_http::watch::spawn(root.to_path_buf(), tx).await;
    // The boot lines `init` just wrote are not a page's doing; let them
    // land before the page starts.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    settle(&mut rx).await;

    // A page that does not echo: every frame is one hop from a fetch
    // nothing caused, so the chain never grows.
    let hops = LOOP_AT as usize + 3;
    let seen = refetch_on_log_frames(&state, &mut rx, hops, false).await;
    assert!(seen.iter().all(|c| *c == Some(1)), "{seen:?}");
    settle(&mut rx).await;
    assert!(loop_warnings(&state).await.is_empty());

    // The same page echoing: each hop is one longer than the last.
    let seen = refetch_on_log_frames(&state, &mut rx, hops, true).await;
    let expected: Vec<Option<u32>> = (1..=hops as u32).map(Some).collect();
    assert_eq!(seen, expected);
    settle(&mut rx).await;

    let warned = loop_warnings(&state).await;
    assert_eq!(warned.len(), 1, "{warned:#?}");
    let fields: serde_json::Value =
        serde_json::from_str(warned[0]["fields"].as_str().unwrap()).unwrap();
    assert_eq!(fields["path"], "/api/health");
    assert_eq!(fields["chain"], LOOP_AT);
    assert_eq!(warned[0]["level"], "warn");
    drop(log);
}
