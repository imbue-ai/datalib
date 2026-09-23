//! What a cancel actually stops. The cooperative half — a download
//! hearing the interrupt and checkpointing — is the runner's and the
//! step's business; this is about the other half, the step that does
//! not stop, and what it spawned.

use app_schema::sync_jobs::{JobKind, JobState};
use datalib_core::app_store::AppStore;
use datalib_core::repo::DynAppRepo;
use datalib_http::worker::{run_job, WorkerConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn dag_bin() -> PathBuf {
    let p = PathBuf::from(std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN"));
    assert!(p.is_file(), "{}", p.display());
    p.canonicalize().unwrap()
}

/// Whether a pid still names a live process. Signal 0 checks without
/// sending anything.
fn alive(pid: i32) -> bool {
    // Safety: plain kill(2) with the null signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Wait for what the next assertion is about, so a hang names what never
/// arrived rather than dying on the test's timeout.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A step that will not stop: it ignores the SIGINT the runner forwards,
/// spawns a child of its own — the shape of `qmd_index` running `node
/// qmd embed` — and otherwise runs forever.
fn deaf_step(root: &Path) -> PathBuf {
    let script = root.join("deaf.sh");
    std::fs::write(
        &script,
        r#"
            trap '' INT
            out="$DATALIB_DAG_DATA_ROOT/deaf/out"
            mkdir -p "$out"
            sleep 120 &
            # Written last, so its arrival means the child is up.
            echo $! > "$out/grandchild.pid"
            while :; do sleep 0.2; done
        "#,
    )
    .unwrap();
    script
}

/// Cancelling a job stops the step that would not stop, and takes what
/// the step spawned with it.
///
/// Three things have to line up for that. The worker sends a *second*
/// SIGTERM after the grace rather than going straight to SIGKILL; the
/// runner treats that second signal as "kill the steps"; and each step
/// is in a process group of its own, so killing it reaches its child.
/// Miss any one and the grandchild here outlives the run — which is how
/// a `qmd embed` came to outlive the whole application.
#[tokio::test]
async fn a_cancel_takes_a_step_that_will_not_stop_and_its_child_with_it() {
    let td = tempfile::tempdir().unwrap();
    let root = Arc::new(td.path().to_path_buf());
    let script = deaf_step(&root);
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[[groups]]\nid = \"deaf\"\n\n\
             [[steps]]\ngroup = \"deaf\"\nfunction = \"out\"\ncommand = \"/bin/sh {}\"\n",
            script.display()
        ),
    )
    .unwrap();

    let repo: DynAppRepo = Arc::new(AppStore::open(root.as_path()).await.unwrap());
    let queued = repo.enqueue_job(JobKind::All, None).await.unwrap();
    let job = repo.claim_next_job().await.unwrap().expect("claimed");
    let cfg = WorkerConfig {
        root: root.clone(),
        dag_bin: Some(dag_bin()),
        binary_dir: None,
        progress_tx: tokio::sync::broadcast::channel(16).0,
    };

    let worker = {
        let repo = repo.clone();
        let job = job.clone();
        tokio::spawn(async move { run_job(&repo, &cfg, job).await })
    };

    let pid_file = root.join("deaf/out/grandchild.pid");
    let grandchild =
        || -> Option<i32> { std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok() };
    until("the step to spawn a child of its own", || {
        grandchild().is_some()
    })
    .await;
    let child_pid = grandchild().unwrap();
    assert!(alive(child_pid), "the spawned child should be running");

    // What `POST /api/sync/jobs/{id}/cancel` does.
    repo.request_cancel_job(&queued.id).await.unwrap();
    worker.await.unwrap().unwrap();

    let done = repo.get_job(&queued.id).await.unwrap().unwrap();
    assert_eq!(done.job_state(), Some(JobState::Canceled));

    // The runner has exited, so nothing is left that could signal this.
    // It is reparented and reaped rather than killed instantly, hence a
    // wait rather than a bare assertion.
    until("the step's child to go with the run", || !alive(child_pid)).await;

    // And the run store's sentence is finished. The runner here was
    // killed rather than allowed to drain, so it recorded none of this
    // itself — without the server closing up after it, the Manage screen
    // reads `running` for a run whose processes are all gone.
    let run = datalib_runs::runs(&root, None, 10)
        .await
        .into_iter()
        .find(|r| r.run_id == queued.id)
        .expect("the runner recorded a run");
    assert!(
        run.finished_at_utc.is_some(),
        "a cancelled run must not be left open"
    );
    let snapshot = datalib_runs::snapshot_of(&root, Some(&queued.id)).await;
    let live: Vec<&str> = snapshot
        .steps
        .iter()
        .filter(|s| !datalib_runs::is_terminal(&s.state))
        .map(|s| s.step.as_str())
        .collect();
    assert!(
        live.is_empty(),
        "steps still reading live after the run closed: {live:?}"
    );
}
