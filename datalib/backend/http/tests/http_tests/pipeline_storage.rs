//! `GET /api/pipeline/storage` — bytes on disk, and how fresh they are;
//! `POST /api/pipeline/storage/refresh` walks the disk first.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_http::router;
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

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

/// The same two steps filed under a `[[groups]]` entry.
const GROUPED_CONFIG: &str = r#"
[[groups]]
id = "pdfs"
name = "Scanned PDFs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "ingest"

[[steps]]
group = "pdfs"
function = "render_markdown"
inputs = ["pdfs/ingest"]
"#;

async fn storage(app: &axum::Router, query: &str) -> serde_json::Value {
    answer(app, "GET", &format!("/api/pipeline/storage{query}")).await
}

/// A walk, then the answer.
async fn refresh(app: &axum::Router) -> serde_json::Value {
    answer(app, "POST", "/api/pipeline/storage/refresh").await
}

async fn answer(app: &axum::Router, method: &str, uri: &str) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
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

    let v = refresh(&app).await;
    assert_eq!(tree(&v, "pdfs/ingest")["present"], false);
    assert_eq!(tree(&v, "pdfs/ingest")["bytes"], 0);
    assert_eq!(tree(&v, "pdfs/render_markdown")["present"], false);
}

/// A refresh sees what was written since the last one.
#[tokio::test]
async fn a_refresh_sees_bytes_written_since_the_last_walk() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let app = router(state(td.path()).await);

    // First walk: nothing on disk.
    let before = refresh(&app).await;
    assert_eq!(tree(&before, "pdfs/ingest")["present"], false);
    let root_before = before["root"]["bytes"].as_u64().unwrap();

    // Now something writes — as a sync would.
    std::fs::create_dir_all(td.path().join("pdfs/ingest")).unwrap();
    std::fs::write(
        td.path().join("pdfs/ingest/entities.doltlite_db"),
        vec![7u8; 4096],
    )
    .unwrap();
    std::fs::write(td.path().join("pdfs/ingest/blobs.sqlite"), vec![7u8; 1024]).unwrap();

    let after = refresh(&app).await;
    let raw = tree(&after, "pdfs/ingest");
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
        none["measured_at_utc"],
        serde_json::Value::Null,
        "before any walk the zero is 'not measured', not 'empty disk'"
    );

    let v = refresh(&app).await;
    assert!(v["measured_at_utc"].is_string());
    assert_eq!(v["window_secs"], 300);
    // The root is measured on every walk, so it always has at least the
    // one sample.
    assert_eq!(v["root"]["history"].as_array().unwrap().len(), 1);
    assert_eq!(v["root"]["path"], ".");
}

/// A group's directory is measured as a tree of its own, on the same
/// walk as its steps. The Manage screen's group row reads that series
/// rather than adding two step series sampled at different instants.
#[tokio::test]
async fn a_group_directory_is_a_measured_tree_of_its_own() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), GROUPED_CONFIG).unwrap();
    let app = router(state(td.path()).await);

    // Declared, nothing written: the group has a row and it is absent,
    // like a step's.
    let before = refresh(&app).await;
    assert_eq!(tree(&before, "pdfs")["present"], false);

    std::fs::create_dir_all(td.path().join("pdfs/ingest")).unwrap();
    std::fs::create_dir_all(td.path().join("pdfs/render_markdown")).unwrap();
    std::fs::write(td.path().join("pdfs/ingest/blobs.sqlite"), vec![7u8; 1024]).unwrap();
    std::fs::write(td.path().join("pdfs/render_markdown/a.md"), vec![7u8; 100]).unwrap();

    let after = refresh(&app).await;
    let group = tree(&after, "pdfs");
    assert_eq!(group["present"], true);
    assert_eq!(group["bytes"], 1124, "both steps' trees, under the group");
    assert!(
        group.get("parts").is_none(),
        "the entities/attachments split belongs to the raw store, not the folder above it"
    );
    assert_eq!(tree(&after, "pdfs/ingest")["bytes"], 1024);
    assert_eq!(tree(&after, "pdfs/render_markdown")["bytes"], 100);
    assert!(group["abs"].as_str().unwrap().ends_with("/pdfs"));
}

/// A GET never walks the data root, whatever its query says. A link can
/// send one with the person's cookie: an `<img src>` in an email, a
/// card's argument, a navigation from another site. A walk of a large
/// root is minutes of disk, and the frame it sends makes a table card
/// on that URL ask again.
#[tokio::test]
async fn a_get_never_walks_the_data_root() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let app = router(state(td.path()).await);
    std::fs::create_dir_all(td.path().join("pdfs/ingest")).unwrap();
    std::fs::write(
        td.path().join("pdfs/ingest/entities.doltlite_db"),
        vec![7u8; 4096],
    )
    .unwrap();

    for uri in [
        "/api/pipeline/storage?refresh=1",
        "/api/pipeline/storage?refresh",
        "/api/manage/rows?refresh=1",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("x-datalib-token", TEST_TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Refused by name, so a caller that still asks this way learns
        // where the walk went rather than reading a stale answer.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains("/refresh"),
            "{uri}: the refusal names the POST"
        );
    }

    let v = storage(&app, "").await;
    assert_eq!(
        tree(&v, "pdfs/ingest")["present"],
        false,
        "a GET walked the root: {v}"
    );
}
