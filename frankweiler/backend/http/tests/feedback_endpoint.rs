//! End-to-end integration test for `POST /api/feedback`.
//!
//! Spawns a managed `dolt sql-server` against a throwaway repo, builds
//! the axum router on top of a `DoltRepo`, drives a request through
//! `tower::ServiceExt::oneshot`, and verifies the row landed in the
//! `feedback` table *and* produced its own `dolt log` entry. Mirrors the
//! shape of `frankweiler/backend/core/tests/dolt_backend_integration.rs`.
//!
//! Requires `dolt` on `$PATH`. Skips (prints + passes) otherwise so the
//! inner `cargo test` loop stays unblocked on hosts without dolt.
//!
//! Tagged `requires-dolt` in BUILD.bazel like the existing dolt fixture
//! tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_http::{router, AppState};
use sqlx::Row;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

fn dolt_available() -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join("dolt").is_file() {
                return true;
            }
        }
    }
    false
}

fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn unique_repo_dir() -> PathBuf {
    // pytest-tmp_path-style: mkdtemp gives a guaranteed-unique path
    // (atomic at the kernel level), so parallel test runs can never
    // collide on the same name the way the old pid+nanos suffix could.
    tempfile::TempDir::with_prefix("fw-http-fb-itest-")
        .expect("create tempdir")
        .keep()
}

#[tokio::test]
async fn post_feedback_inserts_and_dolt_commits() {
    if !dolt_available() {
        eprintln!("skipping: `dolt` not on $PATH");
        return;
    }

    let repo_dir = unique_repo_dir();
    let cfg = DoltConfig {
        host: "127.0.0.1".into(),
        port: pick_free_port(),
        user: "root".into(),
        repo_dirname: repo_dir.file_name().unwrap().to_string_lossy().into_owned(),
        binary: None,
    };
    let server = DoltServer::ensure(&repo_dir, &cfg).expect("dolt sql-server ready");
    let url = server.mysql_url();
    let root = Arc::new(repo_dir.parent().unwrap().to_path_buf());

    // DoltRepo::connect runs the feedback DDL itself.
    let dolt = DoltRepo::connect(&url, root.clone())
        .await
        .unwrap_or_else(|e| panic!("connect dolt at {url}: {e}"));
    let pool = dolt.pool().clone();
    let app_state = AppState {
        root,
        repo: Arc::new(dolt),
        dolt_server: Some(Arc::new(server)),
        qmd_daemon: None,
    };
    let app = router(app_state.clone());

    // Drive a POST with a deliberately-typed surface payload so we know
    // the JSON round-trip preserves the discriminator.
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
    let resp = app.clone().oneshot(req).await.unwrap();
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

    // The row should be readable from any pool connection now that the
    // handler ran DOLT_COMMIT. JSON_EXTRACT lets us reach into context_json
    // without parsing on the rust side.
    let row = sqlx::query(
        "SELECT feedback_uuid, sentiment, comment, app_version, git_hash, \
                JSON_UNQUOTE(JSON_EXTRACT(context_json, '$.surface')) AS surface, \
                JSON_UNQUOTE(JSON_EXTRACT(context_json, '$.payload.column')) AS col \
         FROM feedback WHERE feedback_uuid = ?",
    )
    .bind(&feedback_uuid)
    .fetch_one(&pool)
    .await
    .expect("feedback row visible after DOLT_COMMIT");
    let sent: String = row.try_get("sentiment").unwrap();
    let comment: String = row.try_get("comment").unwrap();
    let app_version: String = row.try_get("app_version").unwrap();
    let git_hash: String = row.try_get("git_hash").unwrap();
    let surface_json: String = row.try_get("surface").unwrap();
    let col_json: String = row.try_get("col").unwrap();
    assert_eq!(sent, "up");
    assert!(comment.contains("Author column"));
    assert!(!app_version.is_empty());
    assert!(!git_hash.is_empty());
    assert_eq!(surface_json, "grid_cell");
    assert_eq!(col_json, "author");

    // Verify a dedicated `dolt log` entry was produced.
    let log_row = sqlx::query("SELECT message FROM dolt_log ORDER BY date DESC LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("dolt_log readable");
    let msg: String = log_row.try_get("message").unwrap();
    assert!(
        msg.contains(&feedback_uuid),
        "expected dolt_log message to mention feedback uuid, got {msg:?}"
    );

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
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Cleanup. Drop the app state so the server's Drop fires and we can
    // remove the temp dir.
    drop(app_state);
    drop(pool);
    let _ = std::fs::remove_dir_all(&repo_dir);
}
