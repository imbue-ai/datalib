//! The store end to end: a writer publishes, a reader in another
//! *process's* position sees it, and the coalescing and retention rules
//! hold.

use datalib_runs::{log_after, snapshot, LogRow, MetricRow, Retention, RunWriter, StepRow};

const T0: &str = "2026-08-31T10:00:00+01:00";

fn start(root: &std::path::Path, run_id: &str) -> RunWriter {
    RunWriter::start(root, run_id, run_id, Retention::default()).expect("start the store")
}

fn at(step: &str, state: &str, msg: &str) -> StepRow {
    StepRow {
        step: step.into(),
        state: state.into(),
        attempt: 1,
        msg: Some(msg.into()),
        updated_at: T0.into(),
        ..Default::default()
    }
}

fn metric(step: &str, name: &str, value: i64) -> MetricRow {
    MetricRow {
        step: step.into(),
        name: name.into(),
        labels: String::new(),
        value,
        updated_at: T0.into(),
    }
}

fn line(step: &str, level: &str, msg: &str) -> LogRow {
    LogRow {
        step: Some(step.into()),
        ts: T0.into(),
        level: level.into(),
        msg: msg.into(),
        ..Default::default()
    }
}

/// Dropping the writer flushes and joins, so everything published is on
/// disk by the time the run reports itself finished.
#[tokio::test]
async fn what_is_published_is_readable() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "run-1");
        w.step(at("slack/raw", "running", "conversations.list"));
        w.step(at("slack/rendered_md", "pending", "waiting"));
        w.metric(metric("slack/raw", "rows_upserted", 3));
        w.log(line("slack/raw", "info", "hello"));
    }

    let snap = snapshot(td.path()).await;
    assert_eq!(snap.run_id.as_deref(), Some("run-1"));
    assert!(
        snap.finished_at.is_some(),
        "a dropped writer closes the run"
    );
    assert_eq!(snap.steps.len(), 2, "{snap:?}");
    let fetch = snap.steps.iter().find(|r| r.step == "slack/raw").unwrap();
    assert_eq!(fetch.state, "running");
    assert_eq!(fetch.msg.as_deref(), Some("conversations.list"));
    assert_eq!(snap.metrics.len(), 1);
    assert_eq!(snap.metrics[0].value, 3);

    let log = log_after(td.path(), "run-1", None, 0, 100).await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].msg, "hello");
    assert!(log[0].seq > 0, "the store assigns the sequence number");
}

/// Only the newest tick per step and per metric survives, which is what
/// makes a chatty download cost one row-write per flush rather than
/// thousands. Log lines are the exception: every one is kept, in order.
#[tokio::test]
async fn ticks_coalesce_but_log_lines_do_not() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "run-1");
        for i in 0..500 {
            w.step(at("slack/raw", "running", &format!("tick {i}")));
            w.metric(metric("slack/raw", "done", i));
            w.log(line("slack/raw", "info", &format!("line {i}")));
        }
    }
    let snap = snapshot(td.path()).await;
    assert_eq!(snap.steps.len(), 1);
    assert_eq!(snap.steps[0].msg.as_deref(), Some("tick 499"));
    assert_eq!(snap.metrics.len(), 1);
    assert_eq!(snap.metrics[0].value, 499);

    let log = log_after(td.path(), "run-1", Some("slack/raw"), 0, 1000).await;
    assert_eq!(log.len(), 500);
    assert_eq!(log[0].msg, "line 0");
    assert_eq!(log[499].msg, "line 499");
    assert!(
        log.windows(2).all(|w| w[0].seq < w[1].seq),
        "seq is monotone"
    );
}

/// The tail contract: a reader that remembers the last `seq` it saw
/// gets only what came after.
#[tokio::test]
async fn log_after_resumes_from_a_sequence_number() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "run-1");
        for i in 0..10 {
            w.log(line("a", "info", &format!("line {i}")));
        }
    }
    let first = log_after(td.path(), "run-1", None, 0, 4).await;
    assert_eq!(first.len(), 4);
    let rest = log_after(td.path(), "run-1", None, first[3].seq, 100).await;
    assert_eq!(rest.len(), 6);
    assert_eq!(rest[0].msg, "line 4");
}

/// A terminal state latches. A progress tick that was already in flight
/// when the step finished must not resurrect it as running — which is
/// exactly what a table showing "running" forever after a sync ended
/// would look like.
#[tokio::test]
async fn a_finished_step_is_not_resurrected_by_a_late_tick() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "run-1");
        w.step(at("slack/raw", "succeeded", "done"));
        w.step(at("slack/raw", "running", "a straggler"));
    }
    let snap = snapshot(td.path()).await;
    assert_eq!(snap.steps[0].state, "succeeded");
    assert_eq!(snap.steps[0].msg.as_deref(), Some("done"));
}

/// Runs accumulate rather than replacing each other — that is what makes
/// last week's log readable — and a reader asking for "the run" gets the
/// newest.
#[tokio::test]
async fn runs_accumulate_and_the_snapshot_is_the_newest() {
    let td = tempfile::tempdir().unwrap();
    // Dated runs, so the order is the test's and not the clock's; the
    // window is widened so retention stays out of this test.
    let keep = Retention {
        max_runs: 100,
        max_age_days: 36500,
    };
    {
        let id = "2026-01-01T00:00:00+00:00";
        let w = RunWriter::start(td.path(), id, id, keep).unwrap();
        w.step(at("gone/raw", "succeeded", "old"));
        w.log(line("gone/raw", "info", "from run 1"));
    }
    {
        let id = "2026-01-02T00:00:00+00:00";
        let w = RunWriter::start(td.path(), id, id, keep).unwrap();
        w.step(at("slack/raw", "running", "new"));
    }
    let snap = snapshot(td.path()).await;
    assert_eq!(snap.run_id.as_deref(), Some("2026-01-02T00:00:00+00:00"));
    assert_eq!(snap.steps.len(), 1);
    assert_eq!(snap.steps[0].step, "slack/raw");

    let old = log_after(td.path(), "2026-01-01T00:00:00+00:00", None, 0, 10).await;
    assert_eq!(old.len(), 1, "the earlier run's log is still there");
}

/// Retention by count: the newest `max_runs` survive, including the run
/// being started, and everything filed under a pruned run goes with it.
#[tokio::test]
async fn retention_keeps_the_newest_runs_and_sweeps_their_rows() {
    let td = tempfile::tempdir().unwrap();
    let keep_two = Retention {
        max_runs: 2,
        max_age_days: 3650,
    };
    for day in 1..=4 {
        let id = format!("2026-01-0{day}T00:00:00+00:00");
        let w = RunWriter::start(td.path(), &id, &id, keep_two).unwrap();
        w.step(at("a", "succeeded", "ok"));
        w.log(line("a", "info", &id));
        w.metric(metric("a", "done", day));
    }
    assert!(
        log_after(td.path(), "2026-01-01T00:00:00+00:00", None, 0, 10)
            .await
            .is_empty()
    );
    assert!(
        log_after(td.path(), "2026-01-02T00:00:00+00:00", None, 0, 10)
            .await
            .is_empty()
    );
    assert_eq!(
        log_after(td.path(), "2026-01-03T00:00:00+00:00", None, 0, 10)
            .await
            .len(),
        1
    );
    assert_eq!(
        log_after(td.path(), "2026-01-04T00:00:00+00:00", None, 0, 10)
            .await
            .len(),
        1
    );
    let snap = snapshot(td.path()).await;
    assert_eq!(snap.run_id.as_deref(), Some("2026-01-04T00:00:00+00:00"));
}

/// Retention by age: a run older than the window goes even when the
/// count would have kept it.
#[tokio::test]
async fn retention_drops_runs_older_than_the_window() {
    let td = tempfile::tempdir().unwrap();
    let a_day = Retention {
        max_runs: 100,
        max_age_days: 1,
    };
    {
        let w = RunWriter::start(td.path(), "old", "2000-01-01T00:00:00+00:00", a_day).unwrap();
        w.log(line("a", "info", "ancient"));
    }
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    {
        let _w = RunWriter::start(td.path(), "new", &now, a_day).unwrap();
    }
    assert!(log_after(td.path(), "old", None, 0, 10).await.is_empty());
    assert_eq!(snapshot(td.path()).await.run_id.as_deref(), Some("new"));
}

/// Reading a root that has never synced is not an error — a fresh data
/// root has no store, and the answer is "nothing is running".
#[tokio::test]
async fn a_root_with_no_store_reads_empty() {
    let td = tempfile::tempdir().unwrap();
    assert!(snapshot(td.path()).await.steps.is_empty());
    assert!(log_after(td.path(), "x", None, 0, 10).await.is_empty());
}

/// A file that will not open is replaced rather than fatal: nothing in it
/// is load-bearing, and a run that refused to start over its own log
/// would be the wrong trade.
#[tokio::test]
async fn a_corrupt_store_is_replaced() {
    let td = tempfile::tempdir().unwrap();
    let path = datalib_runs::runs_path(td.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"this is not a database, sqlite or otherwise").unwrap();
    {
        let w = start(td.path(), "run-1");
        w.log(line("a", "info", "after the reset"));
    }
    assert_eq!(log_after(td.path(), "run-1", None, 0, 10).await.len(), 1);
}

/// A store written by another schema version is remade, not migrated
/// and not fatal — the same trade as a corrupt file.
#[tokio::test]
async fn a_store_from_another_schema_version_is_replaced() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "run-1");
        w.log(line("a", "info", "from before"));
    }
    let path = datalib_runs::runs_path(td.path());
    let pool = datalib_runs::open_or_create(&path).await.unwrap();
    sqlx::query("PRAGMA user_version = 1")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    {
        let w = start(td.path(), "run-2");
        w.log(line("a", "info", "after"));
    }
    assert!(log_after(td.path(), "run-1", None, 0, 10).await.is_empty());
    assert_eq!(log_after(td.path(), "run-2", None, 0, 10).await.len(), 1);
}

/// The property the whole design is for: a reader can read *while* the
/// writer is writing, without contending. Here the reader is a separate
/// connection opened per poll, which is what `datalib-http` does.
#[tokio::test]
async fn a_reader_sees_progress_while_the_writer_is_running() {
    let td = tempfile::tempdir().unwrap();
    let w = start(td.path(), "run-1");

    let mut seen = std::collections::BTreeSet::new();
    for i in 0..40 {
        w.metric(metric("slack/raw", "done", i));
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        for m in snapshot(td.path()).await.metrics {
            seen.insert(m.value);
        }
    }
    drop(w);
    assert!(
        seen.len() > 1,
        "a reader polling during a run must see progress move, saw {seen:?}"
    );
}
