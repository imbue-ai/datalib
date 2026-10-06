//! A page that refetches on every `log` frame feeds itself: its request
//! is a log line, the line is the next frame. The server counts the hops
//! (`loop_guard`) and warns once a chain is long enough to be a loop.
//! Played here with the watch, the real log writer and the real router;
//! the page is this test. The watch is fed (`watch::spawn_fed`): the test
//! says when a file moved, so the chain is counted without depending on
//! when the OS would have said so. One test, because the subscriber it
//! installs is the process's only one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::loop_guard::{CAUSE_HEADER, LOOP_AT, TARGET};
use datalib_http::watch::{Feed, RootEvent, RootFrame, Table, Timing};
use datalib_http::{router, ApiToken, AppState};
use datalib_runs::ProcessLogWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tower::ServiceExt;

const TEST_TOKEN: &str = "feedback-loop-test-token";

/// A hang guard, not a wait: the watch is fed, so nothing waits on the OS.
const HEARD_WITHIN: Duration = Duration::from_secs(10);

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

/// Plays a page that refetches on every `log` frame, for `hops` frames,
/// echoing each frame's chain when `echo` is set. The chains it saw.
///
/// The page takes the newest log frame up to a barrier rather than the
/// next one to arrive: fseventsd can hold events back and hand them over
/// in one burst, and then a frame from an earlier commit would pass for
/// this request's and two later commits would arrive as one frame, which
/// leaves the last hop waiting for a frame that already came.
async fn refetch_on_log_frames(
    state: &AppState,
    root: &Path,
    log: &ProcessLogWriter,
    feed: &Feed,
    rx: &mut broadcast::Receiver<RootFrame>,
    hops: usize,
    echo: bool,
) -> Vec<Option<u32>> {
    // The first fetch is the page loading: nothing caused it.
    get(state, "/api/health", None).await;
    let mut seen = Vec::new();
    for _ in 0..hops {
        let frame = settle(root, log, feed, rx)
            .await
            .pop()
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

/// Commit every line logged so far, tell the watch the run store moved,
/// then take every frame that caused; the `log` ones are returned,
/// oldest first. The watch takes moves in the order it is fed, so once a
/// write of the test's own under `system/frontend/` comes back, the run
/// store's has been reported; a burst is sent whole, so what came with
/// it is already queued.
async fn settle(
    root: &Path,
    log: &ProcessLogWriter,
    feed: &Feed,
    rx: &mut broadcast::Receiver<RootFrame>,
) -> Vec<RootFrame> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log.flush().await;
    feed.moved(&root.join("system/runs/runs.sqlite"));
    let barrier = root.join(format!("system/frontend/barrier-{n}.js"));
    std::fs::write(&barrier, "").unwrap();
    feed.moved(&barrier);
    let is_log = |f: &RootFrame| f.event == (RootEvent::TableChanged { table: Table::Log });
    let mut logs = Vec::new();
    let barrier = async {
        loop {
            let f = rx
                .recv()
                .await
                .expect("the channel neither lags nor closes");
            if f.event == RootEvent::FrontendChanged {
                return;
            }
            if is_log(&f) {
                logs.push(f);
            }
        }
    };
    tokio::time::timeout(HEARD_WITHIN, barrier)
        .await
        .expect("the barrier's own write never came back");
    while let Ok(f) = rx.try_recv() {
        if is_log(&f) {
            logs.push(f);
        }
    }
    logs
}

#[tokio::test]
async fn a_page_refetching_on_its_own_echo_is_warned_about_once_the_chain_is_a_loop() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let log = datalib_http::logging::init(root).expect("the store opens in a temp dir");
    let (tx, mut rx) = broadcast::channel(256);
    let state = state(root, tx.clone()).await;
    let (ready, feed) = datalib_http::watch::spawn_fed(root.to_path_buf(), tx, Timing::default());
    tokio::time::timeout(HEARD_WITHIN, ready.wait())
        .await
        .expect("the watch never reported ready");
    // The boot lines `init` just wrote are not a page's doing; let them
    // land before the page starts.
    settle(root, &log, &feed, &mut rx).await;

    // A page that does not echo: every frame is one hop from a fetch
    // nothing caused, so the chain never grows.
    let hops = LOOP_AT as usize + 3;
    let seen = refetch_on_log_frames(&state, root, &log, &feed, &mut rx, hops, false).await;
    assert!(seen.iter().all(|c| *c == Some(1)), "{seen:?}");
    log.flush().await;
    assert!(loop_warnings(&state).await.is_empty());
    // After the read: its own request is a line too.
    settle(root, &log, &feed, &mut rx).await;

    // The same page echoing: each hop is one longer than the last.
    let seen = refetch_on_log_frames(&state, root, &log, &feed, &mut rx, hops, true).await;
    let expected: Vec<Option<u32>> = (1..=hops as u32).map(Some).collect();
    assert_eq!(seen, expected);
    log.flush().await;

    let warned = loop_warnings(&state).await;
    assert_eq!(warned.len(), 1, "{warned:#?}");
    let fields: serde_json::Value =
        serde_json::from_str(warned[0]["fields"].as_str().unwrap()).unwrap();
    assert_eq!(fields["path"], "/api/health");
    assert_eq!(fields["chain"], LOOP_AT);
    assert_eq!(warned[0]["level"], "warn");
    drop(log);
}
