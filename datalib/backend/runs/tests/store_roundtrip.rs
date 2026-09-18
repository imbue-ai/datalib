//! The store end to end: a writer publishes, a reader in another
//! *process's* position sees it, and the coalescing and retention rules
//! hold.

use datalib_runs::{
    log_after, log_query, snapshot, versions, LogQuery, LogRow, MetricRow, Process,
    ProcessLogWriter, Retention, RunWriter, StepRunRow, StorePart,
};

const T0: &str = "2026-08-31T10:00:00+01:00";

fn start(root: &std::path::Path, run_id: &str) -> RunWriter {
    RunWriter::start(root, run_id, run_id, Retention::default()).expect("start the store")
}

fn at(step: &str, state: &str, msg: &str) -> StepRunRow {
    StepRunRow {
        step: step.into(),
        state: state.into(),
        attempt: 1,
        msg: Some(msg.into()),
        updated_at_utc: T0.into(),
        ..Default::default()
    }
}

fn metric(step: &str, name: &str, value: i64) -> MetricRow {
    MetricRow {
        step: step.into(),
        name: name.into(),
        labels: String::new(),
        value,
        updated_at_utc: T0.into(),
        ..Default::default()
    }
}

fn line(step: &str, level: &str, msg: &str) -> LogRow {
    LogRow {
        step: Some(step.into()),
        ts_utc: T0.into(),
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
        snap.finished_at_utc.is_some(),
        "a dropped writer closes the run"
    );
    assert!(
        snap.finished_at_utc.as_deref().unwrap().ends_with("+00:00"),
        "stamps are stored in UTC: {:?}",
        snap.finished_at_utc
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

/// `latest_metric` answers "what did each step last count?" across
/// runs: the newest run that reported the series wins per step and
/// label, an older run's value never shadows it, and a step that never
/// reported is absent rather than zero.
#[tokio::test]
async fn latest_metric_is_the_newest_report_per_step_and_label() {
    let td = tempfile::tempdir().unwrap();
    let labelled = |step: &str, labels: &str, value: i64| MetricRow {
        labels: labels.into(),
        ..metric(step, "problems", value)
    };
    {
        let w = start(td.path(), "run-1");
        w.metric(labelled("slack/render_markdown", "severity=error", 4));
        w.metric(labelled("slack/render_markdown", "severity=warning", 9));
        w.metric(labelled("mail/render_markdown", "severity=error", 1));
    }
    // A later run: slack re-counted, mail did not run.
    {
        let w = start(td.path(), "run-2");
        w.metric(labelled("slack/render_markdown", "severity=error", 0));
        w.metric(labelled("slack/render_markdown", "severity=warning", 2));
    }
    let latest = datalib_runs::latest_metric(td.path(), "problems").await;
    let find = |step: &str, labels: &str| {
        latest
            .iter()
            .find(|m| m.step == step && m.labels == labels)
            .map(|m| (m.value, m.run_id.clone()))
    };
    assert_eq!(
        find("slack/render_markdown", "severity=error"),
        Some((0, "run-2".into()))
    );
    assert_eq!(
        find("slack/render_markdown", "severity=warning"),
        Some((2, "run-2".into()))
    );
    assert_eq!(
        find("mail/render_markdown", "severity=error"),
        Some((1, "run-1".into())),
        "a step the newer run skipped keeps its last count"
    );
    assert_eq!(find("mail/render_markdown", "severity=warning"), None);
    assert!(datalib_runs::latest_metric(td.path(), "nothing")
        .await
        .is_empty());
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

/// One step's lines across runs come back in run order with the run each
/// line belongs to, the same `seq` cursor tails them, and a `-run:` term
/// drops one run's lines.
#[tokio::test]
async fn log_query_spans_runs_and_reads_terms() {
    let step_log_after =
        |root: &std::path::Path, step: &'static str, after_seq: i64, limit: i64| {
            let root = root.to_path_buf();
            async move {
                log_query(
                    &root,
                    &LogQuery {
                        run: None,
                        step: Some(step),
                        q: "",
                        after_seq,
                        limit,
                    },
                )
                .await
                .unwrap()
            }
        };
    let td = tempfile::tempdir().unwrap();
    let keep = Retention {
        max_runs: 100,
        max_age_days: 36500,
        ..Retention::default()
    };
    for (run, msg) in [("run-1", "first"), ("run-2", "second")] {
        let w = RunWriter::start(td.path(), run, run, keep).unwrap();
        w.log(line("a", "info", msg));
        w.log(line("b", "info", "other step"));
    }
    let a = step_log_after(td.path(), "a", 0, 100).await;
    assert_eq!(
        a.iter()
            .map(|l| (l.run_id.as_deref().unwrap(), l.msg.as_str()))
            .collect::<Vec<_>>(),
        [("run-1", "first"), ("run-2", "second")],
    );
    let tail = step_log_after(td.path(), "a", a[0].seq, 100).await;
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].run_id.as_deref(), Some("run-2"));

    let not_first = log_query(
        td.path(),
        &LogQuery {
            run: None,
            step: Some("a"),
            q: "-run:run-1 sec",
            after_seq: 0,
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(not_first.len(), 1);
    assert_eq!(not_first[0].msg, "second");
    let refused = log_query(
        td.path(),
        &LogQuery {
            run: None,
            step: None,
            q: "author:thad",
            after_seq: 0,
            limit: 100,
        },
    )
    .await;
    assert!(refused.is_err());
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
        ..Retention::default()
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

/// A reader can ask for a run by id, list the runs a step took part in,
/// and read the warn/error count per step — what the Manage screen's
/// history and its E column are made of.
#[tokio::test]
async fn runs_can_be_listed_by_step_and_read_by_id() {
    use datalib_runs::{runs, snapshot_of};
    let td = tempfile::tempdir().unwrap();
    let keep = Retention {
        max_runs: 100,
        max_age_days: 36500,
        ..Retention::default()
    };
    {
        let id = "2026-01-01T00:00:00+00:00";
        let w = RunWriter::start(td.path(), id, id, keep).unwrap();
        w.step(at("a", "succeeded", "ok"));
        w.log(line("a", "warn", "hmm"));
        w.log(line("a", "error", "no"));
        w.log(line("a", "info", "fine"));
    }
    {
        let id = "2026-01-02T00:00:00+00:00";
        let w = RunWriter::start(td.path(), id, id, keep).unwrap();
        w.step(at("b", "succeeded", "ok"));
    }
    let all = runs(td.path(), None, 10).await;
    assert_eq!(
        all.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["2026-01-02T00:00:00+00:00", "2026-01-01T00:00:00+00:00"],
        "newest first"
    );
    let with_a = runs(td.path(), Some("a"), 10).await;
    assert_eq!(with_a.len(), 1);
    assert_eq!(with_a[0].run_id, "2026-01-01T00:00:00+00:00");

    let first = snapshot_of(td.path(), Some("2026-01-01T00:00:00+00:00")).await;
    assert_eq!(first.steps[0].step, "a");
    assert_eq!(first.errors.get("a"), Some(&2), "warn + error, not info");
    assert!(snapshot_of(td.path(), Some("nope")).await.run_id.is_none());
}

/// Retention by count: the newest `max_runs` survive, including the run
/// being started, and everything filed under a pruned run goes with it.
#[tokio::test]
async fn retention_keeps_the_newest_runs_and_sweeps_their_rows() {
    let td = tempfile::tempdir().unwrap();
    let keep_two = Retention {
        max_runs: 2,
        max_age_days: 3650,
        ..Retention::default()
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
        ..Retention::default()
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

/// A rate needs two points. The writer samples a series when its value
/// changed and the floor between samples has passed, and always once
/// more at the end — so a series that moved twice inside the floor still
/// leaves its first and last values. The snapshot carries the newest two
/// per series, oldest first, from the last few minutes only — a rate is
/// a live question, and the query runs on every `manage.rows` frame —
/// and when each step last logged.
#[tokio::test]
async fn the_snapshot_carries_two_recent_samples_per_series_and_the_last_log_time() {
    let td = tempfile::tempdir().unwrap();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let recent = |secs_ago: i64| now.bump_micros(-secs_ago * 1_000_000).to_utc_and_offset().0;
    {
        let w = start(td.path(), "run-1");
        w.metric(MetricRow {
            updated_at_utc: recent(30),
            ..metric("a", "rows", 1)
        });
        // A series that last moved an hour ago has no live rate to give.
        w.metric(MetricRow {
            updated_at_utc: recent(3600),
            ..metric("a", "stale", 100)
        });
        w.log(line("a", "info", "first"));
        // Past the flush interval, inside the sample floor.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        w.metric(MetricRow {
            updated_at_utc: recent(20),
            ..metric("a", "rows", 7)
        });
        w.log(LogRow {
            ts_utc: "2026-09-14T10:00:03.500000+00:00".into(),
            ..line("a", "info", "second")
        });
    }
    let snap = snapshot(td.path()).await;
    let values: Vec<(String, i64)> = snap
        .recent_samples
        .iter()
        .map(|s| (s.name.clone(), s.value))
        .collect();
    assert_eq!(
        values,
        vec![("rows".to_string(), 1), ("rows".to_string(), 7)],
        "{:?}",
        snap.recent_samples
    );
    assert_eq!(
        snap.last_log_at.get("a").map(String::as_str),
        Some("2026-09-14T10:00:03.500000+00:00")
    );
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

async fn wait_for_log_line(root: &std::path::Path, msg: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let all = log_query(
            root,
            &LogQuery {
                run: None,
                step: None,
                q: "",
                after_seq: 0,
                limit: 100,
            },
        )
        .await
        .unwrap_or_default();
        if all.iter().any(|l| l.msg == msg) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "log line {msg:?} never reached the store"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The server's lines share the table with the runs': no `run_id`, the
/// process that wrote them, and the same tail cursor. Both writers hold
/// the file at once, which on plain SQLite is ordinary.
#[tokio::test]
async fn a_process_log_sits_beside_the_runs_and_survives_them() {
    let td = tempfile::tempdir().unwrap();
    // The rows below are stamped on a fixed date; only `max_runs` is
    // under test, so neither age limit may reach them.
    let keep = Retention {
        max_runs: 1,
        max_age_days: 36500,
        process_log_days: 36500,
        ..Retention::default()
    };
    let server = ProcessLogWriter::start(td.path(), Process::Http, keep).unwrap();
    server.log(LogRow {
        ts_utc: "2026-09-15T10:00:00.000000+00:00".into(),
        level: "info".into(),
        target: Some("datalib_http::worker".into()),
        msg: "ready".into(),
        ..Default::default()
    });
    // The line has to be in the file before the runs' are, or `seq`
    // does not read in the order things happened. The server writer
    // flushes on a timer after opening the store, and on a loaded CI
    // runner that can take longer than any sleep chosen here, so wait
    // for the row itself.
    wait_for_log_line(td.path(), "ready").await;
    {
        let w = RunWriter::start(td.path(), "run-1", "2026-09-15T10:00:01+00:00", keep).unwrap();
        w.log(line("a", "warn", "from the run"));
    }
    // A second run under `max_runs: 1` sweeps run-1's rows; the
    // server's must stay, since they belong to no run.
    {
        let w = RunWriter::start(td.path(), "run-2", "2026-09-15T10:00:02+00:00", keep).unwrap();
        w.log(line("a", "info", "from run 2"));
    }
    server.log(LogRow {
        ts_utc: "2026-09-15T10:00:03.000000+00:00".into(),
        level: "warn".into(),
        msg: "still here".into(),
        ..Default::default()
    });
    drop(server);

    let all = log_query(
        td.path(),
        &LogQuery {
            run: None,
            step: None,
            q: "",
            after_seq: 0,
            limit: 100,
        },
    )
    .await
    .unwrap();
    let seen: Vec<(Option<&str>, &str, &str)> = all
        .iter()
        .map(|l| (l.run_id.as_deref(), l.process.as_str(), l.msg.as_str()))
        .collect();
    assert_eq!(
        seen,
        [
            (None, "http", "ready"),
            (Some("run-2"), "dag", "from run 2"),
            (None, "http", "still here"),
        ]
    );

    let servers_only = log_query(
        td.path(),
        &LogQuery {
            run: None,
            step: None,
            q: "process:http",
            after_seq: 0,
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(servers_only.len(), 2);
    assert!(servers_only.iter().all(|l| l.run_id.is_none()));
}

/// A line outside any run has its own age limit, shorter than a run's,
/// and both writers apply it when they open.
#[tokio::test]
async fn old_process_lines_age_out_when_a_writer_opens() {
    let td = tempfile::tempdir().unwrap();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let recent = |secs_ago: i64| now.bump_micros(-secs_ago * 1_000_000).to_utc_and_offset().0;
    let keep = Retention {
        max_runs: 100,
        max_age_days: 30,
        process_log_days: 3,
        ..Retention::default()
    };
    {
        let server = ProcessLogWriter::start(td.path(), Process::Http, keep).unwrap();
        server.log(LogRow {
            ts_utc: "2020-01-01T00:00:00.000000+00:00".into(),
            level: "info".into(),
            msg: "ancient".into(),
            ..Default::default()
        });
        server.log(LogRow {
            ts_utc: recent(60),
            level: "info".into(),
            msg: "fresh".into(),
            ..Default::default()
        });
    }
    {
        let _w = RunWriter::start(td.path(), "run-1", &recent(1), keep).unwrap();
    }
    let all = log_query(
        td.path(),
        &LogQuery {
            run: None,
            step: None,
            q: "process:http",
            after_seq: 0,
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        all.iter().map(|l| l.msg.as_str()).collect::<Vec<_>>(),
        ["fresh"]
    );
}

/// The server's lines are also capped by count, newest kept, so a
/// chatty day at `debug` cannot grow the file past the cap.
#[tokio::test]
async fn process_lines_past_the_cap_go_oldest_first() {
    let td = tempfile::tempdir().unwrap();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let recent = |secs_ago: i64| now.bump_micros(-secs_ago * 1_000_000).to_utc_and_offset().0;
    let keep = Retention {
        process_log_lines: 2,
        ..Retention::default()
    };
    {
        let server = ProcessLogWriter::start(td.path(), Process::Http, keep).unwrap();
        for (i, msg) in ["one", "two", "three"].iter().enumerate() {
            server.log(LogRow {
                ts_utc: recent(30 - i as i64),
                level: "debug".into(),
                msg: msg.to_string(),
                ..Default::default()
            });
        }
    }
    // The cap is applied when a writer opens; a second one does it.
    drop(ProcessLogWriter::start(td.path(), Process::Http, keep).unwrap());
    let all = log_query(
        td.path(),
        &LogQuery {
            run: None,
            step: None,
            q: "process:http",
            after_seq: 0,
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        all.iter().map(|l| l.msg.as_str()).collect::<Vec<_>>(),
        ["two", "three"]
    );
}

/// Every write counts itself under the part of the store it touched,
/// and a run's log lines count apart from the server's — that is what
/// lets a watcher wake the Manage screen for one and not the other.
#[tokio::test]
async fn each_part_of_the_store_counts_its_own_writes() {
    let td = tempfile::tempdir().unwrap();
    assert!(
        versions(td.path()).await.is_empty(),
        "no store, no versions"
    );

    let run_id = "run-1";
    {
        let w = start(td.path(), run_id);
        w.step(at("a", "running", ""));
        w.log(LogRow {
            step: Some("a".into()),
            level: "info".into(),
            msg: "hello".into(),
            ..Default::default()
        });
        w.metric(metric("a", "rows", 1));
    }
    let after_run = versions(td.path()).await;
    for part in [
        StorePart::Runs,
        StorePart::StepRuns,
        StorePart::Metrics,
        StorePart::RunLog,
    ] {
        assert!(
            after_run.get(&part).is_some_and(|v| *v > 0),
            "{part:?}: {after_run:?}"
        );
    }
    assert_eq!(after_run.get(&StorePart::ProcessLog), None);

    {
        let server =
            ProcessLogWriter::start(td.path(), Process::Http, Retention::default()).unwrap();
        server.log(LogRow {
            level: "debug".into(),
            msg: "served".into(),
            ..Default::default()
        });
    }
    let after_server = versions(td.path()).await;
    assert!(after_server
        .get(&StorePart::ProcessLog)
        .is_some_and(|v| *v > 0));
    for part in [
        StorePart::Runs,
        StorePart::StepRuns,
        StorePart::Metrics,
        StorePart::RunLog,
    ] {
        assert_eq!(
            after_server.get(&part),
            after_run.get(&part),
            "{part:?} moved for a server line"
        );
    }
}
