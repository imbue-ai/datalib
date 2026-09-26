//! The server's sync path, end to end: `POST /api/requests` writes a
//! request, the loop the server runs in-process takes it on, and the
//! Manage rows read what the loop records of each step.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_http::{router, ApiToken, AppState};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const TOKEN: &str = "sync-loop-test-token";

async fn server(root: &Path) -> AppState {
    datalib_http::build_state(
        root.to_path_buf(),
        None,
        None,
        ApiToken::from_value(TOKEN, root),
    )
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

/// A call the server refuses: its status and the reason it gives.
async fn refused(state: &AppState, uri: &str, body: serde_json::Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
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

async fn sync_as(state: &AppState, source: &str, by: &str) -> String {
    let request = call(
        state,
        "POST",
        "/api/requests",
        Some(serde_json::json!({ "roots": [source], "by": by })),
    )
    .await;
    request["id"].as_str().unwrap().to_string()
}

async fn sync(state: &AppState, source: &str) -> String {
    sync_as(state, source, "ui").await
}

async fn request(state: &AppState, id: &str) -> serde_json::Value {
    let all = call(state, "GET", "/api/requests", None).await;
    all.as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no request {id} in {all}"))
}

/// A step's Manage row.
async fn row(state: &AppState, id: &str) -> serde_json::Value {
    let rows = call(state, "GET", "/api/manage/rows", None).await;
    rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no row {id}"))
}

fn action<'a>(row: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    row["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no {id} action on {row}"))
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
/// run, instead of waiting for it to end. Each row says who it is being
/// run for.
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
    // The POST answers once the loop has taken the request on, so the
    // first rows read after it already offer to stop it.
    assert_eq!(row(&state, "a/out").await["stop_request_id"], a.as_str());
    until("a to start", Duration::from_secs(30), || async {
        started(root, "a")
    })
    .await;
    let b = sync_as(&state, "b/out", "claude").await;
    until(
        "b to start while a is held",
        Duration::from_secs(10),
        || async { started(root, "b") },
    )
    .await;

    until(
        "both rows to read running",
        Duration::from_secs(10),
        || async {
            row(&state, "a/out").await["status"]["key"] == "running"
                && row(&state, "b/out").await["status"]["key"] == "running"
        },
    )
    .await;
    let (ra, rb) = (row(&state, "a/out").await, row(&state, "b/out").await);
    assert_eq!(ra["stop_request_id"], a.as_str(), "{ra}");
    // Named for the sync it stops, and for who started it if not the UI:
    // a row can be part of a sync started anywhere.
    assert_eq!(action(&ra, "stop")["label"], "Stop the sync of a");
    assert_eq!(
        action(&rb, "stop")["label"],
        "Stop the sync of b, started by claude"
    );
    let dag = call(&state, "GET", "/api/dag", None).await;
    assert_eq!(dag["run"]["live"], true, "{dag}");
    assert_eq!(
        ra["live_run_id"], dag["run"]["run_id"],
        "one run serves both"
    );
    assert_eq!(
        rb["live_run_id"], dag["run"]["run_id"],
        "one run serves both"
    );

    std::fs::write(root.join("release"), "").unwrap();
    for id in [&a, &b] {
        until(
            "both requests to close",
            Duration::from_secs(30),
            || async { request(&state, id).await["state"] == "done" },
        )
        .await;
    }
    until("the run to end", Duration::from_secs(10), || async {
        call(&state, "GET", "/api/dag", None).await["run"]["live"] == false
    })
    .await;
    let ra = row(&state, "a/out").await;
    assert_eq!(ra["status"]["key"], "succeeded", "{ra}");
    assert_eq!(action(&ra, "sync")["enabled"], true, "{ra}");
    state.sync.shutdown(Duration::from_secs(5)).await;
}

/// A Sync pressed while the same sync is open is that sync: the POST
/// answers with the open request rather than opening a second, which the
/// loop would run once more when the first ends.
#[tokio::test]
async fn a_sync_of_steps_already_syncing_is_the_sync_already_open() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(
        root.join("config.toml"),
        source(root, "a", HELD) + &source(root, "b", HELD),
    )
    .unwrap();
    let state = server(root).await;

    let first = sync(&state, "a/out").await;
    assert_eq!(sync(&state, "a/out").await, first);
    let both = call(
        &state,
        "POST",
        "/api/requests",
        Some(serde_json::json!({ "roots": ["a/out", "b/out"] })),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(both, first, "other steps are another sync");
    let open = call(&state, "GET", "/api/requests", None).await;
    let open: Vec<&serde_json::Value> = open
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["state"] == "open")
        .collect();
    assert_eq!(open.len(), 2, "{open:?}");

    std::fs::write(root.join("release"), "").unwrap();
    state.sync.shutdown(Duration::from_secs(5)).await;
}

/// Waits until the loop's record and requests satisfy `ready`, looking
/// again each time a commit is announced, never on a timer.
async fn when(
    store: &datalib_dag::supervisor::store::Store,
    heard: &mut datalib_dag::supervisor::announce::Listener,
    what: &str,
    ready: impl Fn(
        &datalib_dag::supervisor::record::Record,
        &[datalib_dag::supervisor::store::RequestRow],
    ) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let record = store.load_record().await.unwrap();
        let requests = store.recent_requests(100).await.unwrap();
        if ready(&record, &requests) {
            return;
        }
        tokio::time::timeout_at(deadline, heard.next())
            .await
            .unwrap_or_else(|_| panic!("no {what} within 30s"));
    }
}

fn running(record: &datalib_dag::supervisor::record::Record, step: &str) -> bool {
    record
        .steps
        .get(step)
        .and_then(|s| s.state)
        .is_some_and(|k| k.as_str() == "running")
}

/// A source added to `config.toml` on disk mid-sync, by the app's editor,
/// an agent or a person, starts beside the sync already running: the
/// server's watch sees the file move and tells the loop, which takes the
/// new config on. The whole chain, the platform's file events included.
#[tokio::test]
async fn a_source_added_on_disk_mid_sync_starts_beside_the_running_one() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(root.join("config.toml"), source(root, "a", HELD)).unwrap();
    let state = server(root).await;
    let store = datalib_dag::supervisor::store::Store::open(root)
        .await
        .unwrap();
    let mut heard = datalib_dag::supervisor::announce::Listener::new(&store, "test");

    let a = sync(&state, "a/out").await;
    when(&store, &mut heard, "a to run", |r, _| running(r, "a/out")).await;
    // As an editor or an agent writes it: a temp file renamed into place.
    let tmp = root.join("config.tmp");
    std::fs::write(&tmp, source(root, "a", HELD) + &source(root, "b", HELD)).unwrap();
    std::fs::rename(&tmp, root.join("config.toml")).unwrap();
    let b = sync(&state, "b/out").await;
    when(&store, &mut heard, "b to run beside a", |r, _| {
        running(r, "b/out") && running(r, "a/out")
    })
    .await;

    std::fs::write(root.join("release"), "").unwrap();
    when(&store, &mut heard, "both syncs to finish", |_, requests| {
        [&a, &b]
            .iter()
            .all(|id| requests.iter().any(|r| &&r.id == id && r.closed.is_some()))
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

/// Stopping a request stops the step that would not stop, and takes what
/// the step spawned with it. The row reads Stopping, and takes no second
/// click, while that takes: a row that said it was over while its step
/// still held the store would be a lie a second click could act on.
#[tokio::test]
async fn a_stop_takes_a_step_that_will_not_stop_and_its_child_with_it() {
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

    call(&state, "POST", &format!("/api/requests/{id}/stop"), None).await;
    until(
        "the row to read Stopping",
        Duration::from_secs(10),
        || async {
            let stop = row(&state, "deaf/out").await;
            let stop = action(&stop, "stop");
            stop["label"] == "Stopping the sync" && stop["enabled"] == false
        },
    )
    .await;
    until("the request to close", Duration::from_secs(10), || async {
        request(&state, &id).await["state"] == "stopped"
    })
    .await;

    until(
        "the step to have exited",
        Duration::from_secs(60),
        || async { row(&state, "deaf/out").await["status"]["key"] != "running" },
    )
    .await;
    let done = row(&state, "deaf/out").await;
    assert_eq!(done["status"]["key"], "stopped", "{done}");
    assert_eq!(action(&done, "sync")["enabled"], true, "{done}");
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
    let run = done["last_run_id"].as_str().expect("the row names its run");
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

/// A config the loop cannot read refuses the sync with what is wrong
/// with it, whether the request names its sources or asks for all of
/// them — and opens no run to say so.
#[tokio::test]
async fn a_config_the_loop_cannot_read_refuses_the_request_and_says_why() {
    let td = tempfile::tempdir().unwrap();
    let root: PathBuf = td.path().to_path_buf();
    // A top-level key the loader does not know is a fatal diagnostic.
    std::fs::write(
        root.join("config.toml"),
        "no_such_key = 1\n\n[[groups]]\nid = \"unified_index\"\n",
    )
    .unwrap();
    let state = server(&root).await;

    for body in [
        serde_json::json!({}),
        serde_json::json!({ "roots": ["a/out"] }),
    ] {
        let (status, why) = refused(&state, "/api/requests", body).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("no_such_key"), "{why}");
    }
    let open = call(&state, "GET", "/api/requests", None).await;
    assert_eq!(open, serde_json::json!([]));
    assert!(datalib_runs::runs(&root, None, 10).await.is_empty());
    state.sync.shutdown(Duration::from_secs(5)).await;
}

/// A step turned off while nothing syncs reads off at once, saying who;
/// a sync of it then closes without running it, and turning it on lifts
/// that.
#[tokio::test]
async fn a_step_turned_off_reads_off_and_does_not_run() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(root.join("config.toml"), source(root, "a", HELD)).unwrap();
    let state = server(root).await;

    call(
        &state,
        "POST",
        "/api/steps/a%2Fout/turn_off",
        Some(serde_json::json!({ "by": "claude" })),
    )
    .await;
    until("the row to read off", Duration::from_secs(10), || async {
        row(&state, "a/out").await["status"]["key"] == "off"
    })
    .await;
    let turned_off = row(&state, "a/out").await;
    assert_eq!(turned_off["turned_off_by"], "claude", "{turned_off}");
    assert_eq!(turned_off["status"]["label"], "Off", "{turned_off}");
    assert_eq!(
        turned_off["status"]["detail"], "turned off by claude",
        "{turned_off}"
    );
    // The row's switch reads off, and so does its group's: every step
    // under it is off.
    let switch = action(&turned_off, "in_syncs");
    assert_eq!(switch["on"], false, "{turned_off}");
    assert!(switch["hint"]
        .as_str()
        .unwrap()
        .contains("claude turned it off"));
    let group = row(&state, "group:a").await;
    assert_eq!(group["turned_off_by"], "claude", "{group}");
    assert_eq!(action(&group, "in_syncs")["on"], false, "{group}");
    assert_eq!(action(&group, "in_syncs")["enabled"], true, "{group}");

    let id = sync(&state, "a/out").await;
    until("the request to close", Duration::from_secs(30), || async {
        request(&state, &id).await["state"] == "done"
    })
    .await;
    assert!(!started(root, "a"), "a step turned off ran");

    call(&state, "POST", "/api/steps/a%2Fout/turn_on", None).await;
    until("the row to read on", Duration::from_secs(10), || async {
        let r = row(&state, "a/out").await;
        r["status"]["key"] == "never_run"
            && r["turned_off_by"].is_null()
            && r["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == "in_syncs" && a["on"] == true)
    })
    .await;
    state.sync.shutdown(Duration::from_secs(5)).await;
}

/// A step and one that reads it, each counting its runs in a file named
/// for its function. `version` goes on the reader's argv, so changing it
/// changes the reader's fingerprint and nothing else.
fn chain(root: &Path, version: &str) -> String {
    let script = |function: &str| {
        let path = root.join(format!("{function}.sh"));
        std::fs::write(
            &path,
            format!(
                "mkdir -p \"$DATALIB_DAG_DATA_ROOT/$DATALIB_DAG_STEP\"\n\
                 echo run >> \"$DATALIB_DAG_DATA_ROOT/runs-{function}\"\n"
            ),
        )
        .unwrap();
        path.display().to_string()
    };
    format!(
        "[[groups]]\nid = \"a\"\n\n\
         [[steps]]\ngroup = \"a\"\nfunction = \"out\"\ncommand = \"/bin/sh {}\"\n\n\
         [[steps]]\ngroup = \"a\"\nfunction = \"derived\"\ncommand = \"/bin/sh {} {version}\"\n\
         inputs = [\"a/out\"]\n",
        script("out"),
        script("derived"),
    )
}

fn runs(root: &Path, function: &str) -> usize {
    std::fs::read_to_string(root.join(format!("runs-{function}"))).map_or(0, |s| s.lines().count())
}

/// Sync on a step that reads another used to be disabled outright, so a
/// render whose code moved could only rerun behind a fresh download. It
/// is offered once the step is out of date, and reruns it alone.
#[tokio::test]
async fn a_derived_step_out_of_date_syncs_alone() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    std::fs::write(root.join("config.toml"), chain(root, "v1")).unwrap();
    let state = server(root).await;

    let id = sync(&state, "a/out").await;
    until(
        "the first sync to close",
        Duration::from_secs(30),
        || async { request(&state, &id).await["state"] == "done" },
    )
    .await;
    assert_eq!((runs(root, "out"), runs(root, "derived")), (1, 1));
    until(
        "the derived step's Sync to read up to date",
        Duration::from_secs(10),
        || async { action(&row(&state, "a/derived").await, "sync")["enabled"] == false },
    )
    .await;
    let up_to_date = row(&state, "a/derived").await;
    assert!(
        action(&up_to_date, "sync")["disabled_reason"]
            .as_str()
            .unwrap()
            .starts_with("Up to date"),
        "{up_to_date}"
    );

    std::fs::write(root.join("config.toml"), chain(root, "v2")).unwrap();
    until(
        "the derived step's Sync to read out of date",
        Duration::from_secs(10),
        || async { action(&row(&state, "a/derived").await, "sync")["enabled"] == true },
    )
    .await;

    let id = sync(&state, "a/derived").await;
    until(
        "the second sync to close",
        Duration::from_secs(30),
        || async { request(&state, &id).await["state"] == "done" },
    )
    .await;
    assert_eq!(
        (runs(root, "out"), runs(root, "derived")),
        (1, 2),
        "the derived step reran and its input did not"
    );
    state.sync.shutdown(Duration::from_secs(5)).await;
}
