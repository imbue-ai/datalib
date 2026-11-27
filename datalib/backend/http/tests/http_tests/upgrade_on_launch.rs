//! Every launch on a root: the server's loop runs every step whose program
//! takes the verb with `--prepare` appended, starts no other run of a step
//! whose `--prepare` has not exited or of what reads one, and runs the
//! steps whose `--prepare` reported `needs_rerun`
//! (`dag/README.md` §"Upgrading a root").

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use datalib_http::{router, ApiToken, AppState};
use tower::ServiceExt;

const TOKEN: &str = "upgrade-on-launch-test-token";

/// Stands in for `datalib-step` on the binary path: logs how it was
/// invoked, and as `b`'s render's `--prepare` reports `needs_rerun`. While
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
    let supervisor_store = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    let roots: Vec<String> = groups.iter().map(|g| format!("{g}/ingest")).collect();
    supervisor_store.open_request(&roots, "ui").await.unwrap();
    supervisor_store.close().await;
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

/// Every step's `--prepare` runs, producers first, and no step syncs
/// before its own `--prepare` and those of every step it reads have
/// exited; the next launch runs every step's `--prepare` again, since the
/// runner cannot tell whether a program changed in between. `b`'s render,
/// whose `--prepare` reports `needs_rerun`, is run on that launch without
/// anyone pressing Sync on it — only `a` is synced — and leaves the list
/// of what is behind once it has.
#[tokio::test]
async fn a_launch_prepares_every_step_before_it_or_a_reader_syncs_at_every_start() {
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
    let mut prepared: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| l.starts_with("prepare"))
        .collect();
    prepared.sort();
    assert_eq!(
        prepared,
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
        for function in ["ingest", "render_markdown"] {
            assert!(
                at(&format!("prepare {group}/{function}")) < rendered,
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
    let mut prepared: Vec<String> = invoked(&root)
        .into_iter()
        .filter(|l| l.starts_with("prepare"))
        .collect();
    prepared.sort();
    assert_eq!(
        prepared,
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

/// While the downloads' `--prepare`s are still running, the run's rows in
/// the run store already list every step the pass will run `--prepare` for, the rest as
/// pending, so a person can see how many are left (#1678: the pass's run
/// listed only the steps reached).
#[tokio::test]
async fn the_prepare_run_lists_every_step_it_will_prepare_from_the_start() {
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
    let want: Vec<(String, &str)> = vec![
        ("a/ingest".into(), "running"),
        ("a/render_markdown".into(), "pending"),
        ("b/ingest".into(), "running"),
        ("b/render_markdown".into(), "pending"),
    ];
    let deadline = Instant::now() + Duration::from_secs(30);
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
            "while the downloads prepare, the run's rows never read {want:?}: {:?} {got:?}",
            snap.kind
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    std::fs::remove_file(root.join("hold")).unwrap();
    state.sync.shutdown(Duration::from_secs(10)).await;
}

/// While the pass runs, Sync stays offered. A sync of a source the pass
/// has not reached waits for it, its step reading as waiting on the
/// upgrade, and a step the pass is not keeping from starting runs at once.
/// Before, every Sync was disabled until the whole pass was over, and the
/// loop served no request at all until then.
#[tokio::test]
async fn a_sync_waits_only_for_the_steps_the_pass_has_not_reached() {
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

    let supervisor_store = datalib_dag::supervisor::store::Store::open(&root)
        .await
        .unwrap();
    let id = supervisor_store
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
    while supervisor_store.load_record().await.unwrap().steps["a/ingest"].waiting_for
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
    assert_eq!(
        supervisor_store.request(&id).await.unwrap().unwrap().closed,
        None
    );

    std::fs::remove_file(root.join("hold")).unwrap();
    until_invoked(&root, "sync a/ingest").await;
    supervisor_store.close().await;
    state.sync.shutdown(Duration::from_secs(10)).await;
}

/// Two sources have no path between them, so their `--prepare`s run at the
/// same time: with every `--prepare` kept waiting by the `hold` file, both
/// downloads' have started, and neither render's has. The pass once ran one
/// at a time, so `b`'s waited for `a`'s and everything downstream of it.
#[tokio::test]
async fn the_prepares_of_unrelated_steps_run_at_the_same_time() {
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
    until_invoked(&root, "prepare a/ingest").await;
    until_invoked(&root, "prepare b/ingest").await;
    let started = invoked(&root);
    for render in ["prepare a/render_markdown", "prepare b/render_markdown"] {
        assert!(
            !started.iter().any(|l| l == render),
            "a render's --prepare started before its download's exited: {started:?}"
        );
    }
    std::fs::remove_file(root.join("hold")).unwrap();
    until_invoked(&root, "prepare a/render_markdown").await;
    until_invoked(&root, "prepare b/render_markdown").await;
    state.sync.shutdown(Duration::from_secs(10)).await;
}

/// A root with no config yet has no step to run `--prepare` for, so its
/// pass is over at once. The first-run screen waits for `settled`; when the pass moved
/// into the loop, a loop that could not start for want of a config never
/// said it, and the screen waited for good.
#[tokio::test]
async fn a_root_with_no_config_ends_its_pass_at_once() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let bin = bin_dir_with_fake_step(td.path());
    let state = boot(&root, &bin).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while config(&state).await["upgrade"]["settled"] != true {
        assert!(
            Instant::now() < deadline,
            "the pass never settled: {}",
            config(&state).await["upgrade"]
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(config(&state).await["upgrade"]["migrating"], false);
    state.sync.shutdown(Duration::from_secs(10)).await;
}
