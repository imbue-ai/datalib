//! `GET /api/pipeline/storage` — bytes on disk, and how fresh they are.
//!
//! The numbers come from a background walk on a tick, not from a walk
//! per request: that is what stops the cost of the answer scaling with
//! the number of open tabs, and it is what makes a *history* exist at
//! all (see `datalib_http::usage`). The price is that the answer can be
//! a few seconds old, and there are two moments where a few seconds old
//! is wrong rather than merely stale — a page's first paint, and a sync
//! going terminal. `?refresh=1` covers both.
//!
//! The freshness test below is not hypothetical. A first cut debounced
//! the refresh against the last walk's *start*, which meant a walk that
//! began before a sync finished writing would swallow the refresh that
//! came after it — and the Pipeline table's size column read "—" one
//! frame after a sync it had just watched succeed.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "pipeline-storage-test-token";

const CONFIG: &str = r#"
[[steps]]
id = "pdfs/raw"
command = "datalib-step download pdf"

[[steps]]
id = "pdfs/rendered_md"
command = "datalib-step render pdf"
inputs = ["pdfs/raw"]
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
        // Deliberately no sampler task: these tests drive the walk
        // through the endpoint, which is the path under test.
        usage: Default::default(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn storage(app: &axum::Router, query: &str) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/pipeline/storage{query}"))
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
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

fn tree(v: &serde_json::Value, path: &str) -> serde_json::Value {
    v["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|o| o["path"] == path)
        .unwrap_or_else(|| panic!("no output row for {path} in {v}"))
        .clone()
}

/// Every declared step gets a row whether or not anything has written
/// it, and one that hasn't is `present: false` rather than absent from
/// the list — the UI draws that as "—", which is a different fact from
/// "0 B".
#[tokio::test]
async fn a_step_that_has_written_nothing_is_present_false() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let app = router(state(td.path()).await);

    let v = storage(&app, "?refresh=1").await;
    assert_eq!(tree(&v, "pdfs/raw")["present"], false);
    assert_eq!(tree(&v, "pdfs/raw")["bytes"], 0);
    assert_eq!(tree(&v, "pdfs/rendered_md")["present"], false);
}

/// A refresh sees what was written since the last one.
///
/// This is the contract the Pipeline table's size column rests on: a
/// sync finishes, the UI asks with `refresh=1`, and the answer includes
/// the bytes that sync just wrote. A refresh that could be coalesced
/// away by an *earlier* walk would fail here.
#[tokio::test]
async fn a_refresh_sees_bytes_written_since_the_last_walk() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let app = router(state(td.path()).await);

    // First walk: nothing on disk.
    let before = storage(&app, "?refresh=1").await;
    assert_eq!(tree(&before, "pdfs/raw")["present"], false);
    let root_before = before["root"]["bytes"].as_u64().unwrap();

    // Now something writes — as a sync would.
    std::fs::create_dir_all(td.path().join("pdfs/raw")).unwrap();
    std::fs::write(
        td.path().join("pdfs/raw/entities.doltlite_db"),
        vec![7u8; 4096],
    )
    .unwrap();
    std::fs::write(
        td.path().join("pdfs/raw/blobs.doltlite_db"),
        vec![7u8; 1024],
    )
    .unwrap();

    let after = storage(&app, "?refresh=1").await;
    let raw = tree(&after, "pdfs/raw");
    assert_eq!(raw["present"], true);
    assert_eq!(raw["bytes"], 5120);
    // The raw store's split — attachments dwarf the entity rows on a
    // real source, and that is usually the answer to "why is this so
    // big".
    let parts = raw["parts"].as_array().expect("a raw store splits in two");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["label"], "attachments");
    assert_eq!(parts[1]["bytes"], 1024);

    // The root counts it too — it is every byte under the root, not the
    // sum of the declared trees.
    assert!(
        after["root"]["bytes"].as_u64().unwrap() >= root_before + 5120,
        "the root total must include what a step just wrote"
    );
}

/// The response carries the history the sparklines draw, and says how
/// far back it reaches. A caller must read the window rather than
/// assume one — the plot and the data would otherwise be free to
/// disagree about what "recent" means.
#[tokio::test]
async fn the_response_carries_a_history_and_names_its_window() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let app = router(state(td.path()).await);

    let none = storage(&app, "").await;
    assert_eq!(
        none["measured_at"],
        serde_json::Value::Null,
        "before any walk the zero is 'not measured', not 'empty disk'"
    );

    // The spellings a person, a curl or an agent actually writes. A
    // `bool` field here answers 400 to `?refresh=1` — rejecting the
    // whole request over a flag, rather than merely ignoring it.
    for q in ["?refresh=1", "?refresh=true", "?refresh"] {
        assert!(
            storage(&app, q).await["measured_at"].is_string(),
            "{q} should have walked"
        );
    }

    let v = storage(&app, "?refresh=1").await;
    assert!(v["measured_at"].is_string());
    assert_eq!(v["window_secs"], 300);
    // The root is measured on every walk, so it always has at least the
    // one sample.
    assert_eq!(v["root"]["history"].as_array().unwrap().len(), 1);
    assert_eq!(v["root"]["path"], ".");
}
