//! A config in an old shape — a `qmd_index` step, or the index group
//! still named `unified_index` — is rewritten by the server itself, at
//! boot and on a hand edit, with the old text kept as `config.toml.bak`;
//! at boot the index's tree and the loop's record of it follow.

use datalib_http::ApiToken;
use std::path::Path;
use std::time::{Duration, Instant};

const SHARED_QMD_INDEX: &str = r#"# my sources
[[groups]]
id = "mail"
type = "email"

[[steps]]
group = "mail"
function = "ingest"
[steps.params.mbox]
path = "/nowhere/mail.mbox"

[[steps]]
group = "mail"
function = "render_markdown"
inputs = ["mail/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["mail/render_markdown"]
"#;

async fn boot(root: &Path) -> datalib_http::AppState {
    datalib_http::build_state(
        root.to_path_buf(),
        None,
        None,
        ApiToken::from_value("config-upgrade-test-token", root),
        None,
    )
    .await
    .expect("the server boots")
}

fn is_upgraded(text: &str) -> bool {
    text.contains("function = \"qmd_aggregator\"")
        && text.contains("function = \"keyword_index\"")
        && text.contains("function = \"embed\"")
        && !text.contains("function = \"qmd_index\"")
}

/// The scaffold as a build before the rename wrote it.
const UNIFIED_INDEX: &str = r#"# my sources
[[groups]]
id = "mail"
type = "email"

[[steps]]
group = "mail"
function = "ingest"
[steps.params.mbox]
path = "/nowhere/mail.mbox"

[[steps]]
group = "mail"
function = "render_markdown"
inputs = ["mail/ingest"]

[[steps]]
group = "mail"
function = "keyword_index"
inputs = ["mail/render_markdown"]

[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["mail/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_aggregator"
inputs = ["mail/keyword_index"]

[[steps]]
group = "unified_index"
function = "embedding_map"
inputs = ["unified_index/qmd_aggregator"]
"#;

/// A root from before the rename keeps its index and what a person set:
/// the config is renamed, the tree is moved rather than rebuilt, and a
/// step turned off stays off under its new id.
#[tokio::test]
async fn boot_brings_a_unified_index_root_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("config.toml"), UNIFIED_INDEX).unwrap();
    let kept = root.join("unified_index/qmd_aggregator/qmd/index.sqlite");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, "embeddings").unwrap();
    let store = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    store
        .turn_off("unified_index/embedding_map", "test")
        .await
        .unwrap();
    store.close().await;

    let _state = boot(root).await;

    let text = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(text.starts_with("# my sources"), "{text}");
    assert!(!text.contains("\"unified_index"), "{text}");
    let check = datalib_dag::config::check_text(&text);
    assert!(check.is_clean(), "{:?}\n{text}", check.diagnostics);
    assert!(check
        .cfg
        .steps
        .iter()
        .any(|s| s.id == "unified_document_index/embedding_map"));

    assert!(!root.join("unified_index").exists());
    assert_eq!(
        std::fs::read_to_string(
            root.join("unified_document_index/qmd_aggregator/qmd/index.sqlite")
        )
        .unwrap(),
        "embeddings"
    );

    let store = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    let off = store.turned_off().await.unwrap();
    store.close().await;
    assert_eq!(
        off.keys().collect::<Vec<_>>(),
        ["unified_document_index/embedding_map"]
    );
}

#[tokio::test]
async fn boot_upgrades_the_config_and_keeps_the_old_one() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, SHARED_QMD_INDEX).unwrap();

    let _state = boot(tmp.path()).await;

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(is_upgraded(&text), "{text}");
    assert!(
        text.starts_with("# my sources"),
        "a text edit keeps the file: {text}"
    );
    let check = datalib_dag::config::check_text(&text);
    assert!(check.is_clean(), "{:?}\n{text}", check.diagnostics);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml.bak")).unwrap(),
        SHARED_QMD_INDEX
    );
}

/// The file a person or an agent writes while the server runs is upgraded
/// too, not only the one it found at boot.
#[tokio::test]
async fn a_hand_edit_in_the_old_shape_is_upgraded_while_running() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    let _state = boot(tmp.path()).await;

    // Boot does not wait for the root watcher to be listening, so the edit
    // is made again, now and then, until it is seen.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut next_edit = Instant::now();
    loop {
        let text = std::fs::read_to_string(&config).unwrap_or_default();
        if is_upgraded(&text) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the config was never upgraded:\n{text}"
        );
        if Instant::now() >= next_edit {
            std::fs::write(&config, SHARED_QMD_INDEX).unwrap();
            next_edit = Instant::now() + Duration::from_secs(1);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml.bak")).unwrap(),
        SHARED_QMD_INDEX
    );
}

/// A time-series source from before `render_time_series`: a shape the
/// loader still takes, so a save can write it.
const NO_TIME_SERIES_INDEX: &str = r#"# my sources
[[groups]]
id = "watch"
type = "garmin"

[[steps]]
group = "watch"
function = "ingest"
[steps.params.api]
since = "2369-04-01"

[[steps]]
group = "watch"
function = "render_markdown"
inputs = ["watch/ingest"]
"#;

/// A save in an old shape is on disk in the current one by the time the
/// PUT answers. Left to the watcher, a rewrite could read one save and
/// write it back over the next: the TNG fixture lost its last source.
#[tokio::test]
async fn a_save_in_the_old_shape_lands_upgraded() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let tmp = tempfile::tempdir().unwrap();
    let app = datalib_http::router(crate::support::state(tmp.path()).await);
    let body = serde_json::json!({ "text": NO_TIME_SERIES_INDEX }).to_string();
    let resp = app
        .oneshot(
            Request::put("/api/config")
                .header("x-datalib-token", crate::support::TEST_TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let verdict: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(verdict["ok"], true, "{verdict}");

    let text = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
    assert!(text.contains("function = \"render_time_series\""), "{text}");
    assert!(
        text.contains("id = \"unified_time_series_index\""),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml.bak")).unwrap(),
        NO_TIME_SERIES_INDEX
    );
}
