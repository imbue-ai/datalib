//! The bus end to end: a writer publishes, a reader in another
//! *process's* position sees it, and the coalescing rules hold.

use datalib_progress::{snapshot, ProgressWriter, StepProgress};

fn at(step: &str, state: &str, done: Option<i64>, msg: &str) -> StepProgress {
    StepProgress {
        step: step.into(),
        state: state.into(),
        done,
        total: Some(9),
        msg: Some(msg.into()),
        updated_at: "2026-08-31T10:00:00+01:00".into(),
    }
}

/// Dropping the writer flushes and joins, so everything published is on
/// disk by the time the run reports itself finished.
#[tokio::test]
async fn what_is_published_is_readable() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = ProgressWriter::start(td.path(), "run-1").expect("start the bus");
        w.update(at("slack/raw", "running", Some(3), "conversations.list"));
        w.update(at("slack/rendered_md", "pending", None, "waiting"));
    }

    let rows = snapshot(td.path()).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let fetch = rows.iter().find(|r| r.step == "slack/raw").unwrap();
    assert_eq!(fetch.state, "running");
    assert_eq!(fetch.done, Some(3));
    assert_eq!(fetch.total, Some(9));
    assert_eq!(fetch.msg.as_deref(), Some("conversations.list"));
}

/// Only the newest tick per step survives, which is what makes a chatty
/// download cost one row-write per flush rather than thousands.
#[tokio::test]
async fn ticks_coalesce_to_the_newest() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = ProgressWriter::start(td.path(), "run-1").unwrap();
        for i in 0..500 {
            w.update(at("slack/raw", "running", Some(i), &format!("tick {i}")));
        }
    }
    let rows = snapshot(td.path()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].done, Some(499));
    assert_eq!(rows[0].msg.as_deref(), Some("tick 499"));
}

/// A terminal state latches. A progress tick that was already in flight
/// when the step finished must not resurrect it as running — which is
/// exactly what a table showing "running" forever after a sync ended
/// would look like.
#[tokio::test]
async fn a_finished_step_is_not_resurrected_by_a_late_tick() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = ProgressWriter::start(td.path(), "run-1").unwrap();
        w.update(at("slack/raw", "succeeded", Some(9), "done"));
        w.update(at("slack/raw", "running", Some(7), "a straggler"));
    }
    let rows = snapshot(td.path()).await;
    assert_eq!(rows[0].state, "succeeded");
    assert_eq!(rows[0].msg.as_deref(), Some("done"));
}

/// Every run starts clean: the bus is live state, so a step that ran
/// last time and not this time must not linger.
#[tokio::test]
async fn each_run_starts_clean() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = ProgressWriter::start(td.path(), "run-1").unwrap();
        w.update(at("gone/raw", "succeeded", Some(1), "old"));
    }
    assert_eq!(snapshot(td.path()).await.len(), 1);

    {
        let w = ProgressWriter::start(td.path(), "run-2").unwrap();
        w.update(at("slack/raw", "running", Some(1), "new"));
    }
    let rows = snapshot(td.path()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step, "slack/raw");
}

/// Reading a root that has never synced is not an error — a fresh data
/// root has no bus, and the answer is "nothing is running".
#[tokio::test]
async fn a_root_with_no_bus_reads_empty() {
    let td = tempfile::tempdir().unwrap();
    assert!(snapshot(td.path()).await.is_empty());
}

/// The property the whole design is for: a reader can read *while* the
/// writer is writing, without contending. Here the reader is a separate
/// connection opened per poll, which is what `datalib-http` does.
#[tokio::test]
async fn a_reader_sees_progress_while_the_writer_is_running() {
    let td = tempfile::tempdir().unwrap();
    let w = ProgressWriter::start(td.path(), "run-1").unwrap();

    let mut seen = std::collections::BTreeSet::new();
    for i in 0..40 {
        w.update(at("slack/raw", "running", Some(i), "working"));
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        for row in snapshot(td.path()).await {
            if let Some(d) = row.done {
                seen.insert(d);
            }
        }
    }
    drop(w);
    assert!(
        seen.len() > 1,
        "a reader polling during a run must see progress move, saw {seen:?}"
    );
}
