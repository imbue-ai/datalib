//! The server's sync path, end to end: `POST /api/sync/jobs` writes a
//! job and its request, the loop the server runs in-process takes the
//! request on, and the job row follows it to its end.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_http::{router, ApiToken, AppState};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const TOKEN: &str = "sync-loop-test-token";

async fn server(root: &Path) -> AppState {
    datalib_http::build_state(root.to_path_buf(), None, ApiToken::from_value(TOKEN, root))
        .await
        .expect("the server boots")
}

async fn call(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-datalib-token", TOKEN)
        .header("content-type", "application/json");
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let resp = router(state.clone())
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(status.is_success(), "{method} {uri}: {status}");
    if status == StatusCode::NO_CONTENT {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

async fn sync(state: &AppState, source: &str) -> String {
    let job = call(
        state,
        "POST",
        "/api/sync/jobs",
        Some(serde_json::json!({ "kind": "all", "source_ids": source })),
    )
    .await;
    job["id"].as_str().unwrap().to_string()
}

async fn job(state: &AppState, id: &str) -> serde_json::Value {
    call(state, "GET", &format!("/api/sync/jobs/{id}"), None).await
}

/// Wait for what the next assertion is about, so a hang names what never
/// arrived rather than dying on the test's timeout.
async fn until<F, Fut>(what: &str, within: Duration, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if ready().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A group of one step running `script` with `/bin/sh`.
fn source(root: &Path, group: &str, script: &str) -> String {
    let path = root.join(format!("{group}.sh"));
    std::fs::write(&path, script).unwrap();
    format!(
        "[[groups]]\nid = \"{group}\"\n\n\
         [[steps]]\ngroup = \"{group}\"\nfunction = \"out\"\ncommand = \"/bin/sh {}\"\n\n",
        path.display()
    )
}

/// Says it started, then holds until the test lets go.
const HELD: &str = r#"
    out="$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP"
    mkdir -p "$out"
    touch "$DATALIB_DAG_DATA_ROOT/started-$DATALIB_DAG_GROUP"
    while [ ! -e "$DATALIB_DAG_DATA_ROOT/release" ]; do sleep 0.05; done
    echo done > "$out/f"
"#;

fn started(root: &Path, group: &str) -> bool {
    root.join(format!("started-{group}")).exists()
}

/// The point of the server running the loop: a source synced while
/// another's sync is still going starts at once, beside it, in the same
/// run, instead of waiting for it to end.
#[tokio::test]
async fn a_source_synced_during_anothers_sync_runs_beside_it() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(
        root.join("config.toml"),
        source(root, "a", HELD) + &source(root, "b", HELD),
    )
    .unwrap();
    let state = server(root).await;

    let a = sync(&state, "a/out").await;
    until("a to start", Duration::from_secs(30), || async {
        started(root, "a")
    })
    .await;
    let b = sync(&state, "b/out").await;
    until(
        "b to start while a is held",
        Duration::from_secs(10),
        || async { started(root, "b") },
    )
    .await;

    let (ja, jb) = (job(&state, &a).await, job(&state, &b).await);
    assert_eq!(ja["state"], "running", "{ja}");
    assert_eq!(jb["state"], "running", "{jb}");
    let run = ja["parent_job_id"].as_str().expect("a names its run");
    assert_eq!(jb["parent_job_id"], run, "one run serves both");
    let dag = call(&state, "GET", "/api/dag", None).await;
    assert_eq!(dag["run"]["run_id"], run, "{dag}");
    assert_eq!(dag["run"]["live"], true, "{dag}");

    std::fs::write(root.join("release"), "").unwrap();
    for id in [&a, &b] {
        until("both jobs to finish", Duration::from_secs(30), || async {
            job(&state, id).await["state"] == "done"
        })
        .await;
    }
    until("the run to end", Duration::from_secs(10), || async {
        call(&state, "GET", "/api/dag", None).await["run"]["live"] == false
    })
    .await;
    state.sync.shutdown(Duration::from_secs(5)).await;
}

/// Whether a pid still names a live process. Signal 0 checks without
/// sending anything.
fn alive(pid: i32) -> bool {
    // Safety: plain kill(2) with the null signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// A step that will not stop: it ignores its SIGINT, spawns a child of
/// its own — the shape of `qmd_index` running `node qmd embed` — and
/// otherwise runs forever.
const DEAF: &str = r#"
    trap '' INT
    out="$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP"
    mkdir -p "$out"
    sleep 120 &
    # Written last, so its arrival means the child is up.
    echo $! > "$out/grandchild.pid"
    while :; do sleep 0.2; done
"#;

/// Cancelling a job stops the step that would not stop, and takes what
/// the step spawned with it. The job reads "stopping" while that takes,
/// and canceled once it has: a job that said it was over while its step
/// still held the store would be a lie a second click could act on.
#[tokio::test]
async fn a_cancel_takes_a_step_that_will_not_stop_and_its_child_with_it() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(root.join("config.toml"), source(root, "deaf", DEAF)).unwrap();
    let state = server(root).await;

    let id = sync(&state, "deaf/out").await;
    let pid_file = root.join("deaf/out/grandchild.pid");
    let grandchild =
        || -> Option<i32> { std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok() };
    until(
        "the step to spawn a child of its own",
        Duration::from_secs(30),
        || async { grandchild().is_some() },
    )
    .await;
    let child = grandchild().unwrap();
    assert!(alive(child), "the spawned child should be running");

    call(&state, "POST", &format!("/api/sync/jobs/{id}/cancel"), None).await;
    let stopping = job(&state, &id).await;
    assert_eq!(stopping["stopping"], true, "{stopping}");

    until(
        "the job to be stamped finished",
        Duration::from_secs(60),
        || async { job(&state, &id).await["active"] == false },
    )
    .await;
    let done = job(&state, &id).await;
    assert_eq!(done["state"], "canceled", "{done}");
    until(
        "the step's child to go with it",
        Duration::from_secs(30),
        || async { !alive(child) },
    )
    .await;

    // And the run store's sentence is finished: nothing reads live.
    until("the run to close", Duration::from_secs(10), || async {
        !state.sync.running()
    })
    .await;
    let run = done["parent_job_id"]
        .as_str()
        .expect("the job names its run");
    let snapshot = datalib_runs::snapshot_of(root, Some(run)).await;
    let live: Vec<&str> = snapshot
        .steps
        .iter()
        .filter(|s| !datalib_runs::is_terminal(&s.state))
        .map(|s| s.step.as_str())
        .collect();
    assert!(live.is_empty(), "steps still reading live: {live:?}");
    state.sync.shutdown(Duration::from_secs(5)).await;
}

/// A config the loop cannot read fails the sync with what is wrong with
/// it, whether the job names its sources or asks for all of them — and
/// opens no run to say so.
#[tokio::test]
async fn a_config_the_loop_cannot_read_fails_the_job_and_says_why() {
    let td = tempfile::tempdir().unwrap();
    let root: PathBuf = td.path().to_path_buf();
    // A top-level key the loader does not know is a fatal diagnostic.
    std::fs::write(
        root.join("config.toml"),
        "no_such_key = 1\n\n[[groups]]\nid = \"unified_index\"\n",
    )
    .unwrap();
    let state = server(&root).await;

    let everything = call(
        &state,
        "POST",
        "/api/sync/jobs",
        Some(serde_json::json!({ "kind": "all" })),
    )
    .await;
    let named = sync(&state, "a/out").await;
    for id in [everything["id"].as_str().unwrap(), &named] {
        until("the job to fail", Duration::from_secs(30), || async {
            job(&state, id).await["state"] == "failed"
        })
        .await;
        let error = job(&state, id).await["error"].as_str().unwrap().to_string();
        assert!(error.contains("no_such_key"), "{error}");
    }
    assert!(datalib_runs::runs(&root, None, 10).await.is_empty());
    state.sync.shutdown(Duration::from_secs(5)).await;
}
