//! `POST /api/free_disk_space`, and `POST /api/compact`, its old spelling
//! with `drop_old_versions_on_derived_data`: the loop the server runs asks each step that
//! takes the verbs to `--collect`, or `--compact` when old versions go,
//! all of them when no target is named, and refuses a step that does not
//! take them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_http::{router, ApiToken, AppState};
use tower::ServiceExt;

const TOKEN: &str = "compact-endpoint-test-token";

/// Stands in for `datalib-step` on the binary path, logging how it was
/// invoked.
const FAKE_STEP: &str = r#"#!/bin/sh
case " $* " in
  *" --compact "*) mode=compact ;;
  *" --collect "*) mode=collect ;;
  *" --migrate "*) mode=migrate ;;
  *) mode=sync ;;
esac
echo "$mode $DATALIB_DAG_STEP" >> "$DATALIB_DAG_DATA_ROOT/invoked"
"#;

fn bin_dir_with_fake_step(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("datalib-step");
    std::fs::write(&path, FAKE_STEP).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

const CONFIG: &str = "[[groups]]\nid = \"enterprise\"\ntype = \"calendar\"\n\n\
    [[steps]]\ngroup = \"enterprise\"\nfunction = \"ingest\"\n\n\
    [[steps]]\ngroup = \"enterprise\"\nfunction = \"render_markdown\"\n\
    inputs = [\"enterprise/ingest\"]\n\n\
    [[steps]]\ngroup = \"enterprise\"\nfunction = \"captains_log\"\n\
    command = \"true\"\ninputs = [\"enterprise/render_markdown\"]\n";

/// The steps invoked with `verb`, sorted.
fn asked(root: &Path, verb: &str) -> Vec<String> {
    let prefix = format!("{verb} ");
    let mut asked: Vec<String> = std::fs::read_to_string(root.join("invoked"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .map(str::to_string)
        .collect();
    asked.sort();
    asked
}

async fn boot(root: &Path, bin: &Path) -> AppState {
    let state = datalib_http::build_state(
        root.to_path_buf(),
        Some(bin.to_path_buf()),
        None,
        ApiToken::from_value(TOKEN, root),
        Default::default(),
    )
    .await
    .expect("the server boots");
    // The launch's migrate pass comes first; wait it out.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(root.join("invoked"))
        .unwrap_or_default()
        .contains("migrate enterprise/render_markdown")
    {
        assert!(Instant::now() < deadline, "the launch pass never ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    state
}

async fn post(state: &AppState, route: &str, body: serde_json::Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(route)
        .header("x-datalib-token", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn until_asked(root: &Path, verb: &str, want: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while asked(root, verb) != want {
        assert!(
            Instant::now() < deadline,
            "asked to {verb} {:?}, not {want:?}",
            asked(root, verb)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// No targets asks every built-in step, and not a step with a command of
/// its own, which has never heard of the verb.
#[tokio::test]
async fn compact_with_no_targets_asks_every_step_that_takes_the_verb() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let state = boot(&root, &bin_dir_with_fake_step(td.path())).await;

    let (status, body) = post(&state, "/api/compact", serde_json::json!({})).await;
    assert!(
        status == StatusCode::NO_CONTENT || status == StatusCode::ACCEPTED,
        "{status}: {body}"
    );
    until_asked(
        &root,
        "compact",
        &["enterprise/ingest", "enterprise/render_markdown"],
    )
    .await;
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);
}

/// A named step alone is asked; one the config lacks, or one that does
/// not take the verb, is refused with the reason.
#[tokio::test]
async fn compact_asks_the_named_step_and_refuses_one_it_cannot() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let state = boot(&root, &bin_dir_with_fake_step(td.path())).await;

    let (status, body) = post(
        &state,
        "/api/compact",
        serde_json::json!({"targets": ["enterprise/render_markdown"]}),
    )
    .await;
    assert!(
        status == StatusCode::NO_CONTENT || status == StatusCode::ACCEPTED,
        "{status}: {body}"
    );
    until_asked(&root, "compact", &["enterprise/render_markdown"]).await;

    let (status, body) = post(
        &state,
        "/api/compact",
        serde_json::json!({"targets": ["borg/ingest"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("no step \"borg/ingest\""), "{body}");
    let (status, body) = post(
        &state,
        "/api/compact",
        serde_json::json!({"targets": ["enterprise/captains_log"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("does not take --compact"), "{body}");
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);
}

/// Freeing disk space without dropping old versions asks the same steps
/// with `--collect`, and refuses a step that does not take it by that
/// verb's name.
#[tokio::test]
async fn free_disk_space_collects_every_step_that_takes_the_verb() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let state = boot(&root, &bin_dir_with_fake_step(td.path())).await;

    let (status, body) = post(&state, "/api/free_disk_space", serde_json::json!({})).await;
    assert!(
        status == StatusCode::NO_CONTENT || status == StatusCode::ACCEPTED,
        "{status}: {body}"
    );
    until_asked(
        &root,
        "collect",
        &["enterprise/ingest", "enterprise/render_markdown"],
    )
    .await;
    assert!(asked(&root, "compact").is_empty(), "nothing is folded");

    let (status, body) = post(
        &state,
        "/api/free_disk_space",
        serde_json::json!({"targets": ["enterprise/captains_log"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("does not take --collect"), "{body}");
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);
}

/// `drop_old_versions_on_derived_data` asks with `--compact`.
#[tokio::test]
async fn free_disk_space_dropping_old_versions_compacts() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let state = boot(&root, &bin_dir_with_fake_step(td.path())).await;

    let (status, body) = post(
        &state,
        "/api/free_disk_space",
        serde_json::json!({"drop_old_versions_on_derived_data": true, "targets": ["enterprise/render_markdown"]}),
    )
    .await;
    assert!(
        status == StatusCode::NO_CONTENT || status == StatusCode::ACCEPTED,
        "{status}: {body}"
    );
    until_asked(&root, "compact", &["enterprise/render_markdown"]).await;
    assert!(
        asked(&root, "collect").is_empty(),
        "nothing is only collected"
    );
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);
}
