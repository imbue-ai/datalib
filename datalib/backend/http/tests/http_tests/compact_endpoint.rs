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

/// Like [`FAKE_STEP`], and a collection answers with its store's size;
/// the render's waits for `go` under the root, so the test sees the pass
/// with one step done and the next still running.
const FAKE_STEP_REPORTING: &str = r#"#!/bin/sh
case " $* " in
  *" --collect "*) mode=collect ;;
  *" --migrate "*) mode=migrate ;;
  *) mode=sync ;;
esac
echo "$mode $DATALIB_DAG_STEP" >> "$DATALIB_DAG_DATA_ROOT/invoked"
if [ "$mode" = collect ]; then
  if [ "$DATALIB_DAG_STEP" = enterprise/render_markdown ]; then
    while [ ! -e "$DATALIB_DAG_DATA_ROOT/go" ]; do sleep 0.05; done
  fi
  echo '{"event":"outcome","outputs":[],"store_bytes":{"before":3000,"after":1000}}'
fi
"#;

async fn config(state: &AppState) -> serde_json::Value {
    let req = Request::builder()
        .uri("/api/config")
        .header("x-datalib-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The `free_space` rows as (step, state, bytes freed).
fn rows(free_space: &serde_json::Value) -> Vec<(String, String, Option<u64>)> {
    free_space["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|s| {
                    let freed = s["store_bytes"]
                        .as_object()
                        .map(|b| b["before"].as_u64().unwrap() - b["after"].as_u64().unwrap());
                    (
                        s["step"].as_str().unwrap().to_string(),
                        s["state"].as_str().unwrap().to_string(),
                        freed,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn until_free_space(
    state: &AppState,
    what: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let free_space = config(state).await["free_space"].clone();
        if done(&free_space) {
            return free_space;
        }
        assert!(Instant::now() < deadline, "never {what}: {free_space}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Guards the free-space pass the app could not see move: `/api/config`
/// says how each step it asks stands while it runs, with what each store
/// gave back, and that it is over once it is.
#[tokio::test]
async fn free_disk_space_reports_each_step_as_it_goes() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("datalib-step"), FAKE_STEP_REPORTING).unwrap();
    std::fs::set_permissions(
        bin.join("datalib-step"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let state = boot(&root, &bin).await;
    assert_eq!(
        config(&state).await["free_space"],
        serde_json::Value::Null,
        "nothing asked yet"
    );

    let asking = {
        let state = state.clone();
        tokio::spawn(
            async move { post(&state, "/api/free_disk_space", serde_json::json!({})).await },
        )
    };
    let midway = until_free_space(&state, "reached the render", |f| {
        rows(f).get(1).is_some_and(|r| r.1 == "running")
    })
    .await;
    assert_eq!(midway["kind"], "collect");
    assert_eq!(midway["running"], true);
    assert_eq!(
        rows(&midway),
        [
            (
                "enterprise/ingest".to_string(),
                "done".to_string(),
                Some(2000)
            ),
            (
                "enterprise/render_markdown".to_string(),
                "running".to_string(),
                None
            ),
        ]
    );

    std::fs::write(root.join("go"), "").unwrap();
    let over = until_free_space(&state, "ended", |f| f["running"] == false).await;
    assert_eq!(over["error"], serde_json::Value::Null);
    assert_eq!(
        rows(&over),
        [
            (
                "enterprise/ingest".to_string(),
                "done".to_string(),
                Some(2000)
            ),
            (
                "enterprise/render_markdown".to_string(),
                "done".to_string(),
                Some(2000)
            ),
        ]
    );
    let (status, body) = asking.await.unwrap();
    assert!(
        status == StatusCode::NO_CONTENT || status == StatusCode::ACCEPTED,
        "{status}: {body}"
    );
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);
}
