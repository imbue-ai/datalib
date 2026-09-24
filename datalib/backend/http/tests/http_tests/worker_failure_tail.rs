//! What a job's error says when the runner never got as far as a run.
//! Every line a *step* writes is in the run store; a config the runner
//! refuses is said on the runner's own stderr, and the worker's bounded
//! tail is the only place that reaches the person who clicked Sync.

use app_schema::sync_jobs::{JobKind, JobState};
use datalib_core::app_store::AppStore;
use datalib_core::repo::DynAppRepo;
use datalib_http::worker::{run_job, WorkerConfig};
use std::path::PathBuf;
use std::sync::Arc;

fn dag_bin() -> PathBuf {
    let p = PathBuf::from(std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN"));
    assert!(p.is_file(), "{}", p.display());
    p.canonicalize().unwrap()
}

#[tokio::test]
async fn a_refused_config_reaches_the_job_error_through_the_tail() {
    let td = tempfile::tempdir().unwrap();
    let root = Arc::new(td.path().to_path_buf());
    // A top-level key the loader does not know is a fatal diagnostic:
    // the runner says so and exits without a run.
    std::fs::write(
        root.join("config.toml"),
        "no_such_key = 1\n\n[[groups]]\nid = \"unified_index\"\n",
    )
    .unwrap();

    let repo: DynAppRepo = Arc::new(AppStore::open(root.as_path()).await.unwrap());
    let queued = repo.enqueue_job(JobKind::All, None).await.unwrap();
    let job = repo.claim_next_job().await.unwrap().expect("claimed");
    assert_eq!(job.id, queued.id);

    let cfg = WorkerConfig {
        root: root.clone(),
        dag_bin: Some(dag_bin()),
        binary_dir: None,
        progress_tx: tokio::sync::broadcast::channel(16).0,
    };
    run_job(&repo, &cfg, job).await.unwrap();

    let done = repo.get_job(&queued.id).await.unwrap().unwrap();
    assert_eq!(done.job_state(), Some(JobState::Failed));
    let error = done.error.expect("a failed job says why");
    assert!(error.contains("no_such_key"), "{error}");
    assert!(error.starts_with("datalib-dag exited with"), "{error}");

    // And the store has nothing for this run: the tail is not a second
    // copy of what the store holds, it is the part the store never saw.
    assert!(datalib_runs::runs(&root, None, 10).await.is_empty());
}
