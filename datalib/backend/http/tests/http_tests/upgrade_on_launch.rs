//! Every launch on a root: the server's loop asks every step whose program
//! takes the verb to `--prepare`, holding back only what it has not asked
//! and what reads that, and runs the ones that answer `needs_rerun`
//! (`dag/README.md` §"Upgrading a root").

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use datalib_http::{router, ApiToken, AppState};
use tower::ServiceExt;

const TOKEN: &str = "upgrade-on-launch-test-token";

/// Stands in for `datalib-step` on the binary path: logs how it was
/// invoked, and as `b`'s render answers that it needs to run again. While
/// a `hold` file is in the root, a prepare waits for it to go.
const FAKE_STEP: &str = r#"#!/bin/sh
case " $* " in *" --prepare "*) mode=prepare ;; *) mode=sync ;; esac
echo "$mode $DATALIB_DAG_STEP" >> "$DATALIB_DAG_DATA_ROOT/invoked"
if [ "$mode" = prepare ]; then
    while [ -e "$DATALIB_DAG_DATA_ROOT/hold" ]; do sleep 0.05; done
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

const PROGRAMS: &str = "[[programs]]\ncommand = \"datalib-step\"\n\
    verbs = [\"reset\", \"prepare\", \"compact\", \"collect\", \"check\"]\n\n";

fn source(group: &str) -> String {
    format!(
        "[[groups]]\nid = \"{group}\"\ntype = \"calendar\"\n\n\
         [[steps]]\nid = \"{group}/ingest\"\ncommand = \"datalib-step run ingest calendar\"\n\n\
         [[steps]]\nid = \"{group}/render_markdown\"\n\
         command = \"datalib-step run render_markdown calendar\"\n\
         inputs = [\"{group}/ingest\"]\nreads = \"pinned\"\n\n"
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
        Default::default(),
    )
    .await
    .expect("the server boots")
}

/// A sync of `groups`' downloads, opened before the server is up.
async fn sync_before_boot(root: &Path, groups: &[&str]) {
    let mailbox = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    let roots: Vec<String> = groups.iter().map(|g| format!("{g}/ingest")).collect();
    mailbox.open_request(&roots, "ui").await.unwrap();
    mailbox.close().await;
}

async fn sync_a_and_wait(root: &Path) {
    sync_before_boot(root, &["a"]).await;
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
    get(state, "/api/config").await
}

async fn get(state: &AppState, uri: &str) -> serde_json::Value {
    let req = Request::builder()
        .uri(uri)
        .header("x-datalib-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Every step is asked, producers first, and none syncs before it and
/// every step it reads have been asked; the next launch asks every step
/// again, since the runner cannot tell whether a program changed in
/// between. `b`'s render, which says it needs to run again, is run on that
/// launch without anyone asking — only `a` is synced — and leaves the list
/// of what is behind once it has.
#[tokio::test]
async fn a_launch_asks_every_step_before_it_or_a_reader_syncs_at_every_start() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let bin = bin_dir_with_fake_step(td.path());
    std::fs::write(
        root.join("config.toml"),
        PROGRAMS.to_string() + &source("a") + &source("b"),
    )
    .unwrap();

    sync_before_boot(&root, &["a", "b"]).await;
    let state = boot(&root, &bin).await;
    until_invoked(&root, "sync a/render_markdown").await;
    until_invoked(&root, "sync b/render_markdown").await;
    let lines = invoked(&root);
    let at = |line: &str| {
        lines
            .iter()
            .position(|l| l == line)
            .unwrap_or_else(|| panic!("no {line:?} in {lines:?}"))
    };
    let mut asked: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| l.starts_with("prepare"))
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            "prepare a/ingest",
            "prepare a/render_markdown",
            "prepare b/ingest",
            "prepare b/render_markdown",
        ],
        "every step is asked once: {lines:?}"
    );
    for group in ["a", "b"] {
        let synced = at(&format!("sync {group}/ingest"));
        assert!(
            at(&format!("prepare {group}/ingest")) < synced,
            "a download syncs only once it is asked: {lines:?}"
        );
        let rendered = at(&format!("sync {group}/render_markdown"));
        for asked in ["ingest", "render_markdown"] {
            assert!(
                at(&format!("prepare {group}/{asked}")) < rendered,
                "a render syncs only once it and what it reads are asked: {lines:?}"
            );
        }
    }
    for group in ["a", "b"] {
        assert!(
            at(&format!("prepare {group}/ingest"))
                < at(&format!("prepare {group}/render_markdown")),
            "a render is asked after its raw store: {lines:?}"
        );
    }
    let upgrade = until_caught_up(&state).await;
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
    assert!(state.sync.shutdown(Duration::from_secs(10)).await);

    std::fs::remove_file(root.join("invoked")).unwrap();
    sync_a_and_wait(&root).await;
    let state = boot(&root, &bin).await;
    until_invoked(&root, "sync a/render_markdown").await;
    until_invoked(&root, "sync b/render_markdown").await;
    let mut asked: Vec<String> = invoked(&root)
        .into_iter()
        .filter(|l| l.starts_with("prepare"))
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            "prepare a/ingest",
            "prepare a/render_markdown",
            "prepare b/ingest",
            "prepare b/render_markdown",
        ],
        "the next start asks every step again"
    );
    until_caught_up(&state).await;
    state.sync.shutdown(Duration::from_secs(10)).await;
}

/// The upgrade once nothing is left to re-run, waiting for it.
async fn until_caught_up(state: &AppState) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let upgrade = config(state).await["upgrade"].clone();
        if upgrade["rerender"] == serde_json::json!([]) && upgrade["settled"] == true {
            return upgrade;
        }
        assert!(Instant::now() < deadline, "never caught up: {upgrade}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// While the first step is still preparing, the run's record already lists
/// every step the pass will ask, the rest as pending, so a person can see
/// how many are left (#1678: the pass's run listed only the steps reached).
#[tokio::test]
async fn the_prepare_run_lists_every_step_it_will_ask_from_the_start() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let bin = bin_dir_with_fake_step(td.path());
    std::fs::write(
        root.join("config.toml"),
        PROGRAMS.to_string() + &source("a") + &source("b"),
    )
    .unwrap();
    std::fs::write(root.join("hold"), "").unwrap();

    let state = boot(&root, &bin).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let first = loop {
        if let Some(line) = invoked(&root).first() {
            break line.trim_start_matches("prepare ").to_string();
        }
        assert!(Instant::now() < deadline, "no step was asked to prepare");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let steps = [
        "a/ingest",
        "a/render_markdown",
        "b/ingest",
        "b/render_markdown",
    ];
    let want: Vec<(String, &str)> = steps
        .iter()
        .map(|s| {
            let state = if *s == first { "running" } else { "pending" };
            (s.to_string(), state)
        })
        .collect();
    loop {
        let snap = datalib_runs::snapshot(&root).await.unwrap();
        let mut got: Vec<(String, &str)> = snap
            .steps
            .iter()
            .map(|s| (s.step.clone(), s.state.as_str()))
            .collect();
        got.sort();
        if snap.kind.as_deref() == Some("migrate") && got == want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "while {first} prepares, the run's rows never read {want:?}: {:?} {got:?}",
            snap.kind
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    std::fs::remove_file(root.join("hold")).unwrap();
    state.sync.shutdown(Duration::from_secs(10)).await;
}

/// While the pass runs, Sync stays offered. A sync of a source the pass
/// has not reached waits for it, its step reading as waiting on the
/// upgrade, and a step the pass does not hold runs at once. Before, every
/// Sync was disabled until the whole pass was over, and the loop took no
/// request at all until then.
#[tokio::test]
async fn a_sync_waits_only_for_the_steps_the_pass_holds() {
    use datalib_dag::supervisor::tick::WaitKind;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let bin = bin_dir_with_fake_step(td.path());
    let custom = "[[groups]]\nid = \"c\"\n\n[[steps]]\nid = \"c/out\"\n\
                  command = \"sh -c 'mkdir -p c/out && echo x > c/out/f'\"\n\n";
    std::fs::write(
        root.join("config.toml"),
        PROGRAMS.to_string() + &source("a") + custom,
    )
    .unwrap();
    std::fs::write(root.join("hold"), "").unwrap();

    let state = boot(&root, &bin).await;
    until_invoked(&root, "prepare a/ingest").await;
    assert_eq!(config(&state).await["upgrade"]["migrating"], true);
    let rows = get(&state, "/api/manage/rows").await;
    assert_eq!(rows["sync_paused"], serde_json::Value::Null, "{rows}");

    let mailbox = datalib_dag::supervisor::store::Store::open(&root)
        .await
        .unwrap();
    let id = mailbox
        .open_request(&["a/ingest".into(), "c/out".into()], "ui")
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("c/out/f").exists() {
        assert!(
            Instant::now() < deadline,
            "the step the pass does not hold never ran"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    while mailbox.load_record().await.unwrap().steps["a/ingest"].waiting_for
        != Some(WaitKind::Prepare)
    {
        assert!(
            Instant::now() < deadline,
            "a/ingest never read as waiting on the upgrade"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !invoked(&root).iter().any(|l| l == "sync a/ingest"),
        "{:?}",
        invoked(&root)
    );
    assert_eq!(mailbox.request(&id).await.unwrap().unwrap().closed, None);

    std::fs::remove_file(root.join("hold")).unwrap();
    until_invoked(&root, "sync a/ingest").await;
    mailbox.close().await;
    state.sync.shutdown(Duration::from_secs(10)).await;
}
