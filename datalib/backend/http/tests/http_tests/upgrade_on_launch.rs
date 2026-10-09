//! The first launch of a build on a root: the server asks every step that
//! takes the verb to `--migrate` before its loop takes any request, offers
//! to run the ones that answer `needs_rerun`, and asks nobody on the next
//! launch of the same build (`docs/dev/plans/upgrade_on_launch.md`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use datalib_http::{router, ApiToken, AppState};
use tower::ServiceExt;

const TOKEN: &str = "upgrade-on-launch-test-token";

/// Stands in for `datalib-step` on the binary path: logs how it was
/// invoked, and as `b`'s render answers that it needs to run again.
const FAKE_STEP: &str = r#"#!/bin/sh
case " $* " in *" --migrate "*) mode=migrate ;; *) mode=sync ;; esac
echo "$mode $DATALIB_DAG_STEP" >> "$DATALIB_DAG_DATA_ROOT/invoked"
if [ "$mode" = migrate ]; then
    if [ "$DATALIB_DAG_STEP" = b/render_markdown ]; then
        echo '{"event":"outcome","needs_rerun":true,"outputs":[]}'
    fi
    exit 0
fi
mkdir -p "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP"
echo x > "$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP/f"
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

fn source(group: &str) -> String {
    format!(
        "[[groups]]\nid = \"{group}\"\ntype = \"calendar\"\n\n\
         [[steps]]\ngroup = \"{group}\"\nfunction = \"ingest\"\n\n\
         [[steps]]\ngroup = \"{group}\"\nfunction = \"render_markdown\"\n\
         inputs = [\"{group}/ingest\"]\n\n"
    )
}

fn invoked(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("invoked"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn boot(root: &Path, bin: &Path) -> AppState {
    datalib_http::build_state(
        root.to_path_buf(),
        Some(bin.to_path_buf()),
        None,
        ApiToken::from_value(TOKEN, root),
    )
    .await
    .expect("the server boots")
}

/// A sync of `a`, opened before the server is up, and how long to wait
/// for its render to have run.
async fn sync_a_and_wait(root: &Path) {
    let mailbox = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    mailbox
        .open_request(&["a/ingest".to_string()], "ui")
        .await
        .unwrap();
    mailbox.close().await;
}

async fn until_invoked(root: &Path, line: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !invoked(root).iter().any(|l| l == line) {
        assert!(
            Instant::now() < deadline,
            "never saw {line:?}: {:?}",
            invoked(root)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

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

/// Every step is asked, producers first, before the sync opened at boot
/// runs. `b`'s render, which said it needs to run again, is left alone by
/// `a`'s sync and offered; the next launch of the same build asks nobody.
#[tokio::test]
async fn a_launch_asks_every_step_once_per_build_before_any_sync() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let bin = bin_dir_with_fake_step(td.path());
    std::fs::write(root.join("config.toml"), source("a") + &source("b")).unwrap();

    sync_a_and_wait(&root).await;
    let state = boot(&root, &bin).await;
    until_invoked(&root, "sync a/render_markdown").await;
    let lines = invoked(&root);
    let at = |line: &str| {
        lines
            .iter()
            .position(|l| l == line)
            .unwrap_or_else(|| panic!("no {line:?} in {lines:?}"))
    };
    let (asked, ran) = lines.split_at(4);
    let mut asked = asked.to_vec();
    asked.sort();
    assert_eq!(
        asked,
        [
            "migrate a/ingest",
            "migrate a/render_markdown",
            "migrate b/ingest",
            "migrate b/render_markdown",
        ],
        "every step is asked before anything syncs: {lines:?}"
    );
    assert_eq!(ran, ["sync a/ingest", "sync a/render_markdown"]);
    for group in ["a", "b"] {
        assert!(
            at(&format!("migrate {group}/ingest"))
                < at(&format!("migrate {group}/render_markdown")),
            "a render is asked after its raw store: {lines:?}"
        );
    }
    let upgrade = &config(&state).await["upgrade"];
    assert_eq!(upgrade["migrating"], false, "{upgrade}");
    assert_eq!(upgrade["settled"], true, "{upgrade}");
    assert!(
        upgrade["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "done"),
        "{upgrade}"
    );
    assert_eq!(
        upgrade["rerender"],
        serde_json::json!(["b/render_markdown"])
    );
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);

    std::fs::remove_file(root.join("invoked")).unwrap();
    sync_a_and_wait(&root).await;
    let state = boot(&root, &bin).await;
    until_invoked(&root, "sync a/render_markdown").await;
    assert!(
        !invoked(&root).iter().any(|l| l.starts_with("migrate")),
        "the same build asks once: {:?}",
        invoked(&root)
    );
    assert_eq!(
        config(&state).await["upgrade"]["steps"],
        serde_json::json!([])
    );
    state.sync.shutdown(Duration::from_secs(10)).await;
}
