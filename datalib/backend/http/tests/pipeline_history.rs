//! `GET /api/pipeline/history` — a tree's doltlite commit log, as rows.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "pipeline-history-test-token";

const CONFIG: &str = r#"
[[groups]]
id = "pdfs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "ingest"

[[steps]]
group = "pdfs"
function = "render_markdown"
inputs = ["pdfs/ingest"]
"#;

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
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn get(app: &axum::Router, query: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/pipeline/history{query}"))
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
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, body)
}

/// Writes a store the way a step would: two commits on one table.
/// Returns false when the linked sqlite is not doltlite.
async fn write_store(path: &Path) -> bool {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let dolt: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pragma_function_list WHERE name = 'dolt_commit'")
            .fetch_one(&pool)
            .await
            .unwrap();
    if dolt == 0 {
        pool.close().await;
        return false;
    }
    sqlx::query("CREATE TABLE pdf_files (id INTEGER PRIMARY KEY, name TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT dolt_commit('-Am', 'schema: apply DDL')")
        .execute(&pool)
        .await
        .unwrap();
    for i in 0..3 {
        sqlx::query("INSERT INTO pdf_files (id, name) VALUES (?, 'x')")
            .bind(i)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("SELECT dolt_commit('-Am', 'download pdfs: files=3')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    true
}

#[tokio::test]
async fn a_steps_store_comes_back_newest_first_with_row_counts() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    if !write_store(&td.path().join("pdfs/ingest/entities.doltlite_db")).await {
        return;
    }
    let app = router(state(td.path()).await);

    let (status, v) = get(&app, "?tree=pdfs/ingest").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["tree"], "pdfs/ingest");
    let stores = v["stores"].as_array().unwrap();
    assert_eq!(stores.len(), 1, "{v}");
    assert_eq!(stores[0]["path"], "pdfs/ingest/entities.doltlite_db");
    assert_eq!(stores[0]["truncated"], false);
    let commits = stores[0]["commits"].as_array().unwrap();
    let messages: Vec<&str> = commits
        .iter()
        .map(|c| c["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        messages,
        [
            "download pdfs: files=3",
            "schema: apply DDL",
            "Initialize data repository"
        ]
    );
    let newest = &commits[0];
    assert!(
        newest["date"].as_str().unwrap().ends_with("+00:00"),
        "{newest}"
    );
    assert_eq!(newest["tables"][0]["table"], "pdf_files");
    assert_eq!(newest["tables"][0]["rows"], 3);
    assert_eq!(newest["tables"][0]["added"], 3);
    assert_eq!(commits[2]["parent"], serde_json::Value::Null);
}

/// A group answers with every store under its steps; a step that has
/// written nothing contributes no store rather than an error.
#[tokio::test]
async fn a_group_lists_its_steps_stores() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    if !write_store(&td.path().join("pdfs/ingest/entities.doltlite_db")).await {
        return;
    }
    let app = router(state(td.path()).await);

    let (status, v) = get(&app, "?tree=pdfs").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let paths: Vec<&str> = v["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["pdfs/ingest/entities.doltlite_db"]);

    let (status, v) = get(&app, "?tree=pdfs/render_markdown").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["stores"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn an_undeclared_tree_is_404() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let app = router(state(td.path()).await);
    let (status, _) = get(&app, "?tree=system").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "?tree=../pdfs").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
