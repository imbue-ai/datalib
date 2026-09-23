//! The server's own lines reach the store and come back through the
//! same endpoint as a step's: `tracing::warn!` in this process →
//! `system/runs/runs.sqlite` → `GET /api/log?q=process:http`. One test,
//! because the subscriber it installs is the process's only one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "server-log-test-token";

async fn state(root: &Path) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        usage: Default::default(),
        newer_root: Vec::new(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn get(root: &Path, uri: &str) -> serde_json::Value {
    let app = router(state(root).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn what_the_server_logs_is_served_as_its_own_process() {
    let td = tempfile::tempdir().unwrap();
    let log = datalib_http::logging::init(td.path()).expect("the store opens in a temp dir");

    tracing::warn!(job = "j-1", "worker: claim failed: no such table");
    tracing::info!("not this one");

    // The writer flushes on an interval; dropping it is the final flush.
    drop(log);

    let lines = get(td.path(), "/api/log?q=process:http%20level:warn").await;
    let lines = lines.as_array().unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    // The first thing a server with no commit says — a bazel test has
    // neither the launcher's variable nor the staged `git-hash` — is
    // that it has none, so the empty Commit column explains itself.
    assert_eq!(lines[0]["msg"], datalib_runs::NO_GIT_HASH_ADVICE);
    assert!(lines[0]["git_hash"].is_null());
    let l = &lines[1];
    assert_eq!(l["msg"], "worker: claim failed: no such table");
    assert_eq!(l["process"], "http");
    assert_eq!(l["level"], "warn");
    assert!(l["run_id"].is_null(), "no run: {l}");
    assert!(l["step"].is_null());
    assert_eq!(l["target"], module_path!());
    let fields: serde_json::Value = serde_json::from_str(l["fields"].as_str().unwrap()).unwrap();
    assert_eq!(fields["job"], "j-1");

    // And the same store answers for every line, server or run, at once:
    // the two boot lines (no commit; the filter), the warn, the info.
    let all = get(td.path(), "/api/log").await;
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 4, "{all:?}");
    assert_eq!(all[1]["msg"], "log filter");
    let fields: serde_json::Value =
        serde_json::from_str(all[1]["fields"].as_str().unwrap()).unwrap();
    assert_eq!(fields["filter"], datalib_runs::default_filter());
}
