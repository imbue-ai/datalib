//! End-to-end integration test for `POST /api/feedback`.
//!
//! Opens a doltlite file in a temp directory, builds the axum router on
//! top of a `DoltRepo`, drives a request through `tower::ServiceExt::oneshot`,
//! and verifies the row landed in the `feedback` table.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::{router, ApiToken, AppState};
use sqlx::Row;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// Every route behind the token gate (see `datalib_http::auth`), so
/// each request in this file has to carry it. Stamping the header
/// here keeps the call sites about what they actually assert.
const TEST_TOKEN: &str = "itest-token";

async fn send(app: &axum::Router, mut req: Request<Body>) -> axum::http::Response<Body> {
    req.headers_mut()
        .insert("x-datalib-token", TEST_TOKEN.parse().unwrap());
    app.clone().oneshot(req).await.unwrap()
}

fn unique_db_path() -> PathBuf {
    tempfile::TempDir::with_prefix("datalib-http-fb-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db")
}

#[tokio::test]
async fn post_feedback_inserts_row() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let pool = app.feedback_pool().clone();
    let api_token = ApiToken::from_value(TEST_TOKEN, root.as_path());
    let app_state = AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        // No sampler running here, so the monitor is empty and every
        // tree reports as absent — the state a root nobody has walked
        // is in.
        usage: Default::default(),
        // These endpoints must keep working in a data root that
        // declares no applets, which is every data root by default.
        applets: Arc::new(datalib_http::applets::AppletRegistry::build(
            Vec::new(),
            (*root).clone(),
            None,
        )),
        api_token,
    };
    let app = router(app_state.clone());

    let body = serde_json::json!({
        "sentiment": "up",
        "comment": "Author column shows a UUID instead of a real name",
        "context": {
            "url": "http://localhost:8731/?q=",
            "surface": "grid_cell",
            "dom_path_breadcrumb": [],
            "dom_path_selector": "div.row > span.cell",
            "target_uuids": ["row-1", "row-2"],
            "payload": {
                "column": "author",
                "row_uuids": ["row-1", "row-2"],
                "cell_value": "uuid-1234"
            }
        }
    });
    let req = Request::post("/api/feedback")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = send(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "expected 200, got {:?}",
        resp.status()
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let feedback_uuid = parsed["feedback_uuid"].as_str().unwrap().to_string();
    assert_eq!(feedback_uuid.len(), 36);
    assert!(parsed["created_at"].is_string());

    // Verify the row landed. context_json is stored as TEXT in SQLite so
    // we read it back and parse on the rust side.
    let row = sqlx::query(
        "SELECT feedback_uuid, sentiment, comment, app_version, git_hash, context_json \
         FROM feedback WHERE feedback_uuid = ?",
    )
    .bind(&feedback_uuid)
    .fetch_one(&pool)
    .await
    .expect("feedback row visible after insert");
    let sent: String = row.try_get("sentiment").unwrap();
    let comment: String = row.try_get("comment").unwrap();
    let app_version: String = row.try_get("app_version").unwrap();
    let git_hash: String = row.try_get("git_hash").unwrap();
    let context_json: String = row.try_get("context_json").unwrap();
    let ctx: serde_json::Value = serde_json::from_str(&context_json).unwrap();
    assert_eq!(sent, "up");
    assert!(comment.contains("Author column"));
    assert!(!app_version.is_empty());
    assert!(!git_hash.is_empty());
    assert_eq!(ctx["surface"], "grid_cell");
    assert_eq!(ctx["payload"]["column"], "author");

    // Empty-comment requests are rejected.
    let bad = serde_json::json!({
        "comment": "   ",
        "context": { "url": "", "surface": "page_header",
                     "dom_path_breadcrumb": [], "dom_path_selector": "",
                     "target_uuids": [],
                     "payload": {"entity_kind": "conversation", "entity_uuid": "c-1"} }
    });
    let req = Request::post("/api/feedback")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&bad).unwrap()))
        .unwrap();
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    drop(app_state);
    drop(pool);
    let _ = std::fs::remove_file(&db_path);
}
