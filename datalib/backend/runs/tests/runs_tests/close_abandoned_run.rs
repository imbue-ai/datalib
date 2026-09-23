//! Finishing the sentence for a runner that was killed. A runner that
//! exits closes its own run; one that was SIGKILLed ran no code to do it
//! with, and something that outlived it has to.

use datalib_runs::{
    close_abandoned_run, open_or_create, runs, runs_path, snapshot_of, Retention, RunWriter,
    StepRunRow,
};

const T0: &str = "2026-08-31T10:00:00+01:00";
/// Whatever the caller calls a step that never reported. This crate does
/// not enumerate the scheduler's vocabulary, so the tests do not either.
const STOPPED: &str = "stopped";

fn at(step: &str, state: &str) -> StepRunRow {
    StepRunRow {
        step: step.into(),
        state: state.into(),
        attempt: 1,
        updated_at_utc: T0.into(),
        ..Default::default()
    }
}

/// A run left mid-flight: two steps still live, one already done, and
/// the run never closed.
///
/// The writer is used for the rows and then the run is re-opened by
/// hand, because dropping a `RunWriter` closes its run — which is the
/// whole point of it, and exactly what a SIGKILLed runner never gets to
/// do. Reaching into the table is how the test reproduces that without a
/// second process to kill.
async fn abandoned(root: &std::path::Path, run_id: &str) {
    let writer =
        RunWriter::start(root, run_id, run_id, None, Retention::default()).expect("start the run");
    writer.step(at("a/ingest", "running"));
    writer.step(at("b/ingest", "pending"));
    writer.step(at("c/ingest", "succeeded"));
    drop(writer);

    let pool = open_or_create(&runs_path(root)).await.expect("open");
    sqlx::query("UPDATE runs SET finished_at_utc = NULL WHERE run_id = ?")
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("re-open the run");
    pool.close().await;
}

async fn finished_at(root: &std::path::Path, run_id: &str) -> Option<String> {
    runs(root, None, 10)
        .await
        .into_iter()
        .find(|r| r.run_id == run_id)
        .expect("the run exists")
        .finished_at_utc
}

/// An open run is closed, and every step that was still live takes the
/// state the caller named, with the reason on it. A step that had
/// already reported is left exactly as it was.
#[tokio::test]
async fn it_closes_the_run_and_the_steps_that_never_reported() {
    let td = tempfile::tempdir().unwrap();
    abandoned(td.path(), "r1").await;
    assert_eq!(
        finished_at(td.path(), "r1").await,
        None,
        "open to begin with"
    );

    let closed = close_abandoned_run(td.path(), "r1", STOPPED, "the server closed it")
        .await
        .expect("close");
    assert!(closed.run_was_open);
    assert_eq!(
        closed.steps_closed, 2,
        "the running one and the pending one"
    );

    assert!(finished_at(td.path(), "r1").await.is_some());
    let snapshot = snapshot_of(td.path(), Some("r1")).await;
    let state = |step: &str| {
        snapshot
            .steps
            .iter()
            .find(|s| s.step == step)
            .unwrap_or_else(|| panic!("no {step}"))
            .clone()
    };
    assert_eq!(state("a/ingest").state, STOPPED);
    assert_eq!(state("b/ingest").state, STOPPED);
    assert_eq!(
        state("a/ingest").error.as_deref(),
        Some("the server closed it"),
        "a step the server gave up on says so, so it reads differently \
         from one that stopped itself"
    );
    assert!(state("a/ingest").finished_at_utc.is_some());
    assert_eq!(
        state("c/ingest").state,
        "succeeded",
        "a step that reported is not rewritten"
    );
}

/// Calling it twice is calling it once. The server closes the books on
/// every job it finishes, so the second call is the common case, not an
/// edge one — and it must not restamp a run with a later time.
#[tokio::test]
async fn closing_a_closed_run_changes_nothing() {
    let td = tempfile::tempdir().unwrap();
    abandoned(td.path(), "r1").await;

    let first = close_abandoned_run(td.path(), "r1", STOPPED, "first")
        .await
        .expect("close");
    assert!(first.changed_anything());
    let stamp = finished_at(td.path(), "r1").await.expect("closed");

    let second = close_abandoned_run(td.path(), "r1", STOPPED, "second")
        .await
        .expect("close again");
    assert!(!second.changed_anything(), "{second:?}");
    assert_eq!(
        finished_at(td.path(), "r1").await,
        Some(stamp),
        "the second call must not move the stamp the first one set"
    );
    let snapshot = snapshot_of(td.path(), Some("r1")).await;
    let first_reason = snapshot
        .steps
        .iter()
        .find(|s| s.step == "a/ingest")
        .unwrap()
        .error
        .clone();
    assert_eq!(
        first_reason.as_deref(),
        Some("first"),
        "the reason recorded is the one that actually closed it"
    );
}

/// A run that no longer exists, and a root with no store at all, are
/// both nothing to do rather than an error — the server calls this on
/// every job it finishes, including ones that never reached a run.
#[tokio::test]
async fn a_run_that_is_not_there_is_not_an_error() {
    let td = tempfile::tempdir().unwrap();
    let empty = close_abandoned_run(td.path(), "nope", STOPPED, "why")
        .await
        .expect("a missing store is fine");
    assert!(!empty.changed_anything());

    abandoned(td.path(), "r1").await;
    let unknown = close_abandoned_run(td.path(), "r2", STOPPED, "why")
        .await
        .expect("an unknown run is fine");
    assert!(!unknown.changed_anything());
    assert_eq!(
        finished_at(td.path(), "r1").await,
        None,
        "and it leaves the run that does exist alone"
    );
}
