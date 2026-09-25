//! The server every endpoint test here drives: the app stores open on a
//! temp root, no usage sampler running, and every route behind the one
//! test token, which a request carries in `x-datalib-token`.

use std::path::Path;
use std::sync::Arc;

use tower::ServiceExt;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{ApiToken, AppState};

pub const TEST_TOKEN: &str = "http-test-token";

/// The state for `root`, with the applets its config names.
pub async fn state(root: &Path) -> AppState {
    state_with_applets(root, AppletRegistry::from_data_root(root, None)).await
}

pub async fn state_with_applets(root: &Path, applets: AppletRegistry) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        sync: datalib_http::supervisor::SyncControl::new(root.clone()),
        app: Arc::new(app),
        root_tx: tokio::sync::broadcast::channel(16).0,
        // No sampler running here, so the monitor is empty and every
        // tree reports as absent — the state a root nobody has walked
        // is in.
        usage: Default::default(),
        newer_root: Vec::new(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(applets),
    }
}

/// GET `uri` with the token; the status, and the body as JSON (`Null`
/// when it is not).
pub async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
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
