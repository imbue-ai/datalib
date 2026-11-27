//! The store end to end: a writer publishes, a reader in another
//! *process's* position sees it, and the coalescing and retention rules
//! hold.

use datalib_runs::{
    log_after, log_query, log_values, processes, requests_of_steps, snapshot, snapshot_of,
    versions, InvocationMetricRow, InvocationRow, LogCursor, LogQuery, LogRow, LoopWriter, Process,
    ProcessLogWriter, ProcessRow, Retention, StorePart,
};
use datalib_sql::{test_sql, test_sql_id, Sql};

use crate::request::{now_utc, Request};

const T0: &str = "2026-08-31T10:00:00+01:00";

/// Wide enough that neither limit reaches a test's dated rows.
const KEEP: Retention = Retention {
    max_runs: 100,
    max_age_days: 36500,
    process_log_days: 36500,
    process_log_lines: 200_000,
};

fn start(root: &std::path::Path, id: &str) -> Request {
    Request::start(root, id)
}

fn at(step: &str, state: &str, msg: &str) -> InvocationRow {
    InvocationRow {
        step: step.into(),
        state: state.into(),
        attempt: 1,
        msg: Some(msg.into()),
        updated_at_utc: now_utc(),
        ..Default::default()
    }
}

fn metric(step: &str, name: &str, value: i64) -> InvocationMetricRow {
    InvocationMetricRow {
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
/// disk once it is gone: the invocation, its numbers and its lines, under
/// the request it served.
#[tokio::test]
async fn what_is_published_is_readable() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "r-1");
        w.step(at("slack/raw", "running", "conversations.list"));
        w.metric(metric("slack/raw", "rows_upserted_total", 3));
        w.log(line("slack/raw", "info", "hello"));
    }

    let snap = snapshot_of(td.path(), Some("r-1")).await.unwrap();
    assert_eq!(snap.run_id.as_deref(), Some("r-1"));
    assert!(!snap.old_run);
    assert!(
        snap.started_at_utc.as_deref().unwrap().ends_with("+00:00"),
        "stamps are stored in UTC: {:?}",
        snap.started_at_utc
    );
    assert_eq!(snap.finished_at_utc, None, "its one invocation still runs");
    assert_eq!(snap.steps.len(), 1, "{snap:?}");
    let fetch = &snap.steps[0];
    assert_eq!(fetch.step, "slack/raw");
    assert_eq!(fetch.invocation_id.as_deref(), Some("r-1/slack/raw"));
    assert_eq!(fetch.state, "running");
    assert_eq!(fetch.msg.as_deref(), Some("conversations.list"));
    assert_eq!(snap.metrics.len(), 1);
    assert_eq!(snap.metrics[0].value, 3);
    assert_eq!(snapshot(td.path()).await.unwrap().steps, snap.steps);

    let log = log_after(td.path(), "r-1", None, 0, 100).await.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].msg, "hello");
    assert_eq!(log[0].invocation_id.as_deref(), Some("r-1/slack/raw"));
    assert!(log[0].seq > 0, "the store assigns the sequence number");
}

/// Only the newest tick per invocation and per metric survives, which is
/// what makes a chatty download cost one row-write per flush rather than
/// thousands. Log lines are the exception: every one is kept, in order.
#[tokio::test]
async fn ticks_coalesce_but_log_lines_do_not() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "r-1");
        for i in 0..500 {
            w.step(at("slack/raw", "running", &format!("tick {i}")));
            w.metric(metric("slack/raw", "done_total", i));
            w.log(line("slack/raw", "info", &format!("line {i}")));
        }
    }
    let snap = snapshot(td.path()).await.unwrap();
    assert_eq!(snap.steps.len(), 1);
    assert_eq!(snap.steps[0].msg.as_deref(), Some("tick 499"));
    assert_eq!(snap.metrics.len(), 1);
    assert_eq!(snap.metrics[0].value, 499);

    let log = log_after(td.path(), "r-1", Some("slack/raw"), 0, 1000)
        .await
        .unwrap();
    assert_eq!(log.len(), 500);
    assert_eq!(log[0].msg, "line 0");
    assert_eq!(log[499].msg, "line 499");
    assert!(
        log.windows(2).all(|w| w[0].seq < w[1].seq),
        "seq is monotone"
    );
}

/// `latest_metric` answers "what did each step last count?" across
/// invocations: the newest invocation that reported the series wins per
/// step and label, an older one's value never shadows it, and a step that
/// never reported is absent rather than zero.
#[tokio::test]
async fn latest_metric_is_the_newest_report_per_step_and_label() {
    let td = tempfile::tempdir().unwrap();
    let labelled = |step: &str, labels: &str, value: i64| InvocationMetricRow {
        labels: labels.into(),
        ..metric(step, "problems", value)
    };
    {
        let w = Request::dated(td.path(), "r-1", "2026-09-01T00:00:00.000000+00:00", KEEP);
        w.metric(labelled("slack/render_markdown", "severity=error", 4));
        w.metric(labelled("slack/render_markdown", "severity=warning", 9));
        w.metric(labelled("mail/render_markdown", "severity=error", 1));
    }
    // A later request: slack re-counted, mail did not run.
    {
        let w = Request::dated(td.path(), "r-2", "2026-09-02T00:00:00.000000+00:00", KEEP);
        w.metric(labelled("slack/render_markdown", "severity=error", 0));
        w.metric(labelled("slack/render_markdown", "severity=warning", 2));
    }
    let latest = datalib_runs::latest_metric(td.path(), "problems")
        .await
        .unwrap();
    let find = |step: &str, labels: &str| {
        latest
            .iter()
            .find(|m| m.step == step && m.labels == labels)
            .map(|m| (m.value, m.reported_in.id().to_string()))
    };
    assert_eq!(
        find("slack/render_markdown", "severity=error"),
        Some((0, "r-2/slack/render_markdown".into()))
    );
    assert_eq!(
        find("slack/render_markdown", "severity=warning"),
        Some((2, "r-2/slack/render_markdown".into()))
    );
    assert_eq!(
        find("mail/render_markdown", "severity=error"),
        Some((1, "r-1/mail/render_markdown".into())),
        "a step the newer request did not run keeps its last count"
    );
    assert_eq!(find("mail/render_markdown", "severity=warning"), None);
    assert!(datalib_runs::latest_metric(td.path(), "nothing")
        .await
        .unwrap()
        .is_empty());
}

/// `metric_history` is what a sparkline over several syncs draws: every
/// sample inside the window, oldest first, and the one value the series
/// held when the window opened — without which a series that last moved
/// before it would draw nothing.
#[tokio::test]
async fn metric_history_is_the_window_and_the_value_it_opened_at() {
    let td = tempfile::tempdir().unwrap();
    let at = |stamp: &str, step: &str, value: i64| InvocationMetricRow {
        updated_at_utc: stamp.into(),
        ..metric(step, "items", value)
    };
    for (request, stamp, value) in [
        ("r-1", "2026-09-01T00:00:00.000000+00:00", 10),
        ("r-2", "2026-09-02T00:00:00.000000+00:00", 20),
        ("r-3", "2026-09-03T00:00:00.000000+00:00", 30),
    ] {
        let w = Request::dated(td.path(), request, stamp, KEEP);
        w.metric(at(stamp, "slack/render_markdown", value));
        w.metric(at(stamp, "mail/render_markdown", value * 100));
    }
    let history =
        datalib_runs::metric_history(td.path(), "items", "2026-09-01T12:00:00.000000+00:00")
            .await
            .unwrap();
    let of = |step: &str| {
        history
            .iter()
            .filter(|s| s.step == step)
            .map(|s| s.value)
            .collect::<Vec<_>>()
    };
    assert_eq!(of("slack/render_markdown"), [10, 20, 30]);
    assert_eq!(of("mail/render_markdown"), [1000, 2000, 3000]);
    assert!(
        datalib_runs::metric_history(td.path(), "nothing", "2026-09-01")
            .await
            .unwrap()
            .is_empty()
    );
}

/// The tail contract: a reader that remembers the last `seq` it saw
/// gets only what came after.
#[tokio::test]
async fn log_after_resumes_from_a_sequence_number() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "r-1");
        for i in 0..10 {
            w.log(line("a", "info", &format!("line {i}")));
        }
    }
    let first = log_after(td.path(), "r-1", None, 0, 4).await.unwrap();
    assert_eq!(first.len(), 4);
    let rest = log_after(td.path(), "r-1", None, first[3].seq, 100)
        .await
        .unwrap();
    assert_eq!(rest.len(), 6);
    assert_eq!(rest[0].msg, "line 4");
}

/// A panel opens on the newest lines and pages back from the oldest one it
/// holds; every page reads oldest first, and the first line of the log
/// has nothing before it.
#[tokio::test]
async fn the_newest_lines_come_first_and_pages_read_back_from_them() {
    let td = tempfile::tempdir().unwrap();
    let w = Request::dated(td.path(), "r-1", &now_utc(), KEEP);
    for msg in ["one", "two", "three", "four", "five"] {
        w.log(line("a", "info", msg));
    }
    // Dropping the writer flushes it.
    drop(w);
    let read = |cursor: LogCursor| {
        let root = td.path().to_path_buf();
        async move {
            let q = LogQuery {
                q: "",
                cursor,
                limit: 2,
            };
            let lines = log_query(&root, &q).await.unwrap();
            (
                lines.iter().map(|l| l.msg.clone()).collect::<Vec<_>>(),
                lines.first().map(|l| l.seq),
            )
        }
    };
    let (newest, oldest_held) = read(LogCursor::Newest).await;
    assert_eq!(newest, ["four", "five"]);
    let (before, first) = read(LogCursor::Before(oldest_held.unwrap())).await;
    assert_eq!(before, ["two", "three"]);
    let (earlier, first) = read(LogCursor::Before(first.unwrap())).await;
    assert_eq!(earlier, ["one"]);
    let (nothing, _) = read(LogCursor::Before(first.unwrap())).await;
    assert!(nothing.is_empty());
}

/// One step's lines across requests come back in order with the
/// invocation each line belongs to, the same `seq` cursor tails them, and
/// a `-run:` term drops one request's lines.
#[tokio::test]
async fn log_query_spans_requests_and_reads_terms() {
    let step_log_after =
        |root: &std::path::Path, step: &'static str, after_seq: i64, limit: i64| {
            let root = root.to_path_buf();
            async move {
                log_query(
                    &root,
                    &LogQuery {
                        q: &format!("step:{step}"),
                        cursor: LogCursor::After(after_seq),
                        limit,
                    },
                )
                .await
                .unwrap()
            }
        };
    let td = tempfile::tempdir().unwrap();
    for (request, msg) in [("r-1", "first"), ("r-2", "second")] {
        let w = Request::dated(td.path(), request, &now_utc(), KEEP);
        w.log(line("a", "info", msg));
        w.log(line("b", "info", "other step"));
    }
    let a = step_log_after(td.path(), "a", 0, 100).await;
    assert_eq!(
        a.iter()
            .map(|l| (l.invocation_id.as_deref().unwrap(), l.msg.as_str()))
            .collect::<Vec<_>>(),
        [("r-1/a", "first"), ("r-2/a", "second")],
    );
    let tail = step_log_after(td.path(), "a", a[0].seq, 100).await;
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].invocation_id.as_deref(), Some("r-2/a"));

    let not_first = log_query(
        td.path(),
        &LogQuery {
            q: "step:a -run:r-1 sec",
            cursor: LogCursor::After(0),
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
            q: "author:thad",
            cursor: LogCursor::After(0),
            limit: 100,
        },
    )
    .await;
    assert!(refused.is_err());
}

/// What the log's search bar suggests for a key: the values holding what
/// was typed, among the lines the rest of the query keeps, most lines
/// first; for `min_level:`, the levels.
#[tokio::test]
async fn a_key_suggests_its_values_most_lines_first() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "r-1");
        w.log(line("slack/ingest", "info", "a"));
        w.log(line("slack/ingest", "warn", "b"));
        w.log(line("slack/render_markdown", "info", "c"));
        w.log(line("mail/ingest", "info", "d"));
    }
    let values = |q: &'static str, key: &'static str, typed: &'static str| {
        let root = td.path().to_path_buf();
        async move { log_values(&root, q, key, typed).await.unwrap() }
    };
    let counted = |v: &[(&str, u64)]| -> Vec<(String, Option<u64>)> {
        v.iter().map(|(s, n)| (s.to_string(), Some(*n))).collect()
    };
    assert_eq!(
        values("", "step", "SLACK").await,
        counted(&[("slack/ingest", 2), ("slack/render_markdown", 1)])
    );
    assert_eq!(
        values("level:warn", "step", "").await,
        counted(&[("slack/ingest", 1)]),
        "the rest of the query narrows the values"
    );
    assert_eq!(
        values("", "min_level", "r").await,
        ["trace", "warn", "error"].map(|l| (l.to_string(), None)),
        "in level order"
    );
    assert!(values("", "no_such_key", "").await.is_empty());
    assert!(log_values(td.path(), "author:thad", "step", "")
        .await
        .is_err());
}

/// A terminal state latches. A progress tick that was already in flight
/// when the invocation ended must not resurrect it as running — which is
/// exactly what a table showing "running" forever after a sync ended
/// would look like.
#[tokio::test]
async fn a_finished_invocation_is_not_resurrected_by_a_late_tick() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "r-1");
        w.step(at("slack/raw", "succeeded", "done"));
        w.step(at("slack/raw", "running", "a straggler"));
    }
    let snap = snapshot(td.path()).await.unwrap();
    assert_eq!(snap.steps[0].state, "succeeded");
    assert_eq!(snap.steps[0].msg.as_deref(), Some("done"));
}

/// Requests accumulate rather than replacing each other — that is what
/// makes last week's log readable — and the snapshot with no id is each
/// step's newest invocation, whichever request it served.
#[tokio::test]
async fn invocations_accumulate_and_the_snapshot_is_each_steps_newest() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = Request::dated(td.path(), "r-1", "2026-01-01T00:00:00.000000+00:00", KEEP);
        w.step(at("gone/raw", "succeeded", "old"));
        w.step(at("slack/raw", "succeeded", "old"));
        w.log(line("gone/raw", "info", "from request 1"));
    }
    {
        let w = Request::dated(td.path(), "r-2", "2026-01-02T00:00:00.000000+00:00", KEEP);
        w.step(at("slack/raw", "running", "new"));
    }
    let snap = snapshot(td.path()).await.unwrap();
    assert_eq!(snap.run_id, None);
    let newest: Vec<(&str, Option<&str>)> = snap
        .steps
        .iter()
        .map(|s| (s.step.as_str(), s.invocation_id.as_deref()))
        .collect();
    assert_eq!(
        newest,
        [
            ("gone/raw", Some("r-1/gone/raw")),
            ("slack/raw", Some("r-2/slack/raw"))
        ]
    );
    let second = snapshot_of(td.path(), Some("r-2")).await.unwrap();
    assert_eq!(second.steps.len(), 1);
    assert_eq!(second.steps[0].step, "slack/raw");

    let old = log_after(td.path(), "r-1", None, 0, 10).await.unwrap();
    assert_eq!(old.len(), 1, "the earlier request's log is still there");
}

/// A reader can list the requests a step's invocations served, read one
/// by id, and read the warn/error count per step — what the Manage
/// screen's history and its E column are made of.
#[tokio::test]
async fn requests_can_be_listed_by_step_and_read_by_id() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = Request::dated(td.path(), "r-1", "2026-01-01T00:00:00.000000+00:00", KEEP);
        w.step(at("a", "succeeded", "ok"));
        w.log(line("a", "warn", "hmm"));
        w.log(line("a", "error", "no"));
        w.log(line("a", "info", "fine"));
    }
    {
        let w = Request::dated(td.path(), "r-2", "2026-01-02T00:00:00.000000+00:00", KEEP);
        w.step(at("b", "succeeded", "ok"));
    }
    let ids = |v: Vec<(String, String)>| v.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    assert_eq!(
        ids(requests_of_steps(td.path(), None, 10).await.unwrap()),
        ["r-2", "r-1"],
        "newest first"
    );
    assert_eq!(
        ids(requests_of_steps(td.path(), Some(&["a".to_string()]), 10)
            .await
            .unwrap()),
        ["r-1"]
    );

    let first = snapshot_of(td.path(), Some("r-1")).await.unwrap();
    assert_eq!(first.steps[0].step, "a");
    assert_eq!(first.errors.get("a"), Some(&2), "warn + error, not info");
    assert!(snapshot_of(td.path(), Some("nope"))
        .await
        .unwrap()
        .run_id
        .is_none());
}

/// The requests each writer's open keeps: retention runs as a writer
/// opens, so a test opens one more after writing.
fn apply_retention(root: &std::path::Path, retention: Retention) {
    drop(LoopWriter::start(root, None, retention).unwrap());
}

/// Retention by count: the invocations of the newest `max_runs` requests
/// survive, and everything filed under a pruned invocation goes with it.
#[tokio::test]
async fn retention_keeps_the_newest_requests_and_sweeps_their_rows() {
    let td = tempfile::tempdir().unwrap();
    let keep_two = Retention {
        max_runs: 2,
        ..KEEP
    };
    for day in 1..=4 {
        let id = format!("2026-01-0{day}T00:00:00.000000+00:00");
        let w = Request::dated(td.path(), &id, &id, keep_two);
        w.step(at("a", "succeeded", "ok"));
        w.log(line("a", "info", &id));
        w.metric(metric("a", "done_total", day));
    }
    apply_retention(td.path(), keep_two);
    for (day, lines) in [(1, 0), (2, 0), (3, 1), (4, 1)] {
        let id = format!("2026-01-0{day}T00:00:00.000000+00:00");
        assert_eq!(
            log_after(td.path(), &id, None, 0, 10).await.unwrap().len(),
            lines,
            "{id}"
        );
    }
    let pool = datalib_runs::open_or_create(&datalib_runs::runs_path(td.path()))
        .await
        .unwrap();
    for table in [
        "invocations",
        "invocation_requests",
        "log",
        "invocation_metrics",
        "invocation_metric_samples",
    ] {
        // Safe: `table` is one of the five literals above.
        let kept: Vec<String> = Sql::dynamic(
            test_sql_id!(),
            format!(
                "SELECT DISTINCT invocation_id FROM {table} WHERE invocation_id IS NOT NULL \
                 ORDER BY invocation_id"
            ),
        )
        .query_scalar()
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!kept.is_empty(), "{table} holds the kept invocations' rows");
        assert!(
            kept.iter().all(|r| r.as_str() >= "2026-01-03"),
            "{table} still files rows under a pruned invocation: {kept:?}"
        );
    }
    pool.close().await;
}

/// Requests whose invocations share a stamp, as a test's or a fixture's
/// do: retention still keeps exactly `max_runs` of them.
#[tokio::test]
async fn retention_keeps_max_runs_requests_when_they_share_a_stamp() {
    let td = tempfile::tempdir().unwrap();
    let keep_two = Retention {
        max_runs: 2,
        ..KEEP
    };
    let stamp = "2026-01-01T00:00:00.000000+00:00";
    for id in ["r-c", "r-b", "r-a"] {
        let w = Request::dated(td.path(), id, stamp, keep_two);
        w.step(at("a", "succeeded", id));
    }
    apply_retention(td.path(), keep_two);
    let kept = requests_of_steps(td.path(), None, 10).await.unwrap();
    assert_eq!(kept.len(), 2, "{kept:?}");
}

/// Retention by age: an invocation older than the window goes even when
/// the count would have kept it; one still running stays.
#[tokio::test]
async fn retention_drops_invocations_older_than_the_window() {
    let td = tempfile::tempdir().unwrap();
    let a_day = Retention {
        max_age_days: 1,
        ..KEEP
    };
    {
        let w = Request::dated(td.path(), "old", "2000-01-01T00:00:00.000000+00:00", a_day);
        w.step(at("a", "succeeded", "ancient"));
        w.log(line("a", "info", "ancient"));
        w.step(at("b", "running", "still going"));
    }
    {
        let w = Request::dated(td.path(), "new", &now_utc(), a_day);
        w.step(at("a", "succeeded", "today"));
    }
    apply_retention(td.path(), a_day);
    assert!(log_after(td.path(), "old", None, 0, 10)
        .await
        .unwrap()
        .is_empty());
    let left: Vec<String> = datalib_runs::invocations_of(td.path(), "old")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.invocation_id)
        .collect();
    assert_eq!(left, ["old/b"], "a running invocation is never pruned");
}

/// Reading a root that has never synced is not an error — a fresh data
/// root has no store, and the answer is "nothing is running".
#[tokio::test]
async fn a_root_with_no_store_reads_empty() {
    let td = tempfile::tempdir().unwrap();
    assert!(snapshot(td.path()).await.unwrap().steps.is_empty());
    assert!(log_after(td.path(), "x", None, 0, 10)
        .await
        .unwrap()
        .is_empty());
}

/// A file that will not open is replaced rather than fatal: nothing in it
/// is load-bearing, and a sync that refused to start over its own log
/// would be the wrong trade.
#[tokio::test]
async fn a_corrupt_store_is_replaced() {
    let td = tempfile::tempdir().unwrap();
    let path = datalib_runs::runs_path(td.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"this is not a database, sqlite or otherwise").unwrap();
    {
        let w = start(td.path(), "r-1");
        w.log(line("a", "info", "after the reset"));
    }
    assert_eq!(
        log_after(td.path(), "r-1", None, 0, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// A store that will not open is `None` from `start`, where the caller
/// says nothing will be recorded. It used to be a writer that took every
/// line and kept none, with one WARN on its own thread to say so.
#[test]
fn a_store_that_will_not_open_starts_no_writer() {
    let td = tempfile::tempdir().unwrap();
    let dir = datalib_runs::runs_path(td.path())
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"a file where the store's directory goes").unwrap();
    assert!(LoopWriter::start(td.path(), None, Retention::default()).is_none());
    assert!(
        ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).is_none()
    );
}

/// A store written by another schema version is remade, not migrated
/// and not fatal — the same trade as a corrupt file. The old file is
/// kept beside it first, as plain SQLite a stock `sqlite3` can open.
#[tokio::test]
async fn a_store_from_another_schema_version_is_replaced() {
    let td = tempfile::tempdir().unwrap();
    {
        let w = start(td.path(), "r-1");
        w.log(line("a", "info", "from before"));
    }
    let path = datalib_runs::runs_path(td.path());
    let pool = datalib_runs::open_or_create(&path).await.unwrap();
    test_sql!("PRAGMA user_version = 1")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    {
        let w = start(td.path(), "r-2");
        w.log(line("a", "info", "after"));
    }
    assert!(log_after(td.path(), "r-1", None, 0, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        log_after(td.path(), "r-2", None, 0, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    let dir = path.parent().unwrap();
    let kept: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("runs.bak_")
        })
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let head = std::fs::read(&kept[0]).unwrap();
    assert!(head.starts_with(b"SQLite format 3\0"), "not plain SQLite");
    let copy = datalib_runs::open_or_create(&kept[0]).await.unwrap();
    let msgs: Vec<String> = test_sql!("SELECT msg FROM log")
        .query_scalar()
        .fetch_all(&copy)
        .await
        .unwrap();
    copy.close().await;
    assert_eq!(msgs, ["from before"]);
}

/// Turns a store made by this build into one in version 15's shape: no
/// invocation tables, no `invocation_id` columns, and their indexes gone.
const BACK_TO_15: &str = "DROP INDEX log_by_invocation; \
     DROP INDEX processes_by_invocation; \
     ALTER TABLE log DROP COLUMN invocation_id; \
     ALTER TABLE processes DROP COLUMN invocation_id; \
     DROP TABLE invocations; \
     DROP TABLE invocation_requests; \
     DROP TABLE invocation_metrics; \
     DROP TABLE invocation_metric_samples; \
     PRAGMA user_version = 15";

/// One run of an older build, as that build left it: the run, its two
/// steps, its runner and a step's attempt, a line each, and a metric and
/// its sample.
const AN_OLD_RUN: &str = "INSERT INTO runs (run_id, started_at_utc, finished_at_utc, tz_offset, \
       kind, reason) VALUES ('old-1', '2026-11-01T10:00:00.000000+00:00', \
       '2026-11-01T10:05:00.000000+00:00', '+00:00', 'sync', 'a sync asked for by ui: a'); \
     INSERT INTO step_runs (run_id, step, state, attempt, started_at_utc, finished_at_utc, error, \
       msg, updated_at_utc, tz_offset) VALUES \
       ('old-1', 'a', 'succeeded', 1, '2026-11-01T10:00:01.000000+00:00', \
        '2026-11-01T10:04:00.000000+00:00', NULL, 'done', '2026-11-01T10:04:00.000000+00:00', \
        '+00:00'), \
       ('old-1', 'b', 'failed', 2, '2026-11-01T10:00:02.000000+00:00', \
        '2026-11-01T10:05:00.000000+00:00', 'boom', NULL, '2026-11-01T10:05:00.000000+00:00', \
        '+00:00'); \
     INSERT INTO processes (process_id, process, run_id, step, attempt, started_at_utc, \
       tz_offset) VALUES ('old-dag', 'dag', 'old-1', NULL, NULL, \
       '2026-11-01T10:00:00.000000+00:00', '+00:00'), ('old-b', 'step', 'old-1', 'b', 2, \
       '2026-11-01T10:00:02.000000+00:00', '+00:00'); \
     INSERT INTO log (run_id, process_id, step, attempt, ts_utc, tz_offset, level, msg) VALUES \
       ('old-1', 'old-b', 'b', 2, '2026-11-01T10:04:30.000000+00:00', '+00:00', 'error', \
        'boom'); \
     INSERT INTO metrics (run_id, step, name, labels, value, updated_at_utc, tz_offset) VALUES \
       ('old-1', 'a', 'problems', 'severity=error', 7, '2026-11-01T10:04:00.000000+00:00', \
        '+00:00'), \
       ('old-1', 'b', 'problems', 'severity=error', 3, '2026-11-01T10:05:00.000000+00:00', \
        '+00:00'); \
     INSERT INTO metric_samples (run_id, step, name, labels, ts_utc, tz_offset, value) VALUES \
       ('old-1', 'a', 'problems', 'severity=error', '2026-11-01T10:04:00.000000+00:00', \
        '+00:00', 7)";

/// A store from before invocations climbs to this build's shape in place,
/// and what its runs recorded stays readable as it was written: the run
/// in the old runs' list, its steps by its id, its lines by `run:` and by
/// the run's log, and its metric — until an invocation counts the same
/// series, whose value is then the newer.
#[tokio::test]
async fn an_old_runs_rows_stay_readable_after_the_climb_to_invocations() {
    let td = tempfile::tempdir().unwrap();
    datalib_runs::prepare_store(td.path()).await.unwrap();
    let path = datalib_runs::runs_path(td.path());
    let pool = datalib_runs::open_or_create(&path).await.unwrap();
    Sql::dynamic(test_sql_id!(), BACK_TO_15)
        .raw()
        .execute(&pool)
        .await
        .unwrap();
    Sql::dynamic(test_sql_id!(), AN_OLD_RUN)
        .raw()
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    {
        let w = Request::dated(td.path(), "r-new", &now_utc(), KEEP);
        w.metric(InvocationMetricRow {
            labels: "severity=error".into(),
            ..metric("a", "problems", 1)
        });
    }

    let old = datalib_runs::runs(td.path(), None, 10).await.unwrap();
    assert_eq!(old.len(), 1, "{old:?}");
    assert_eq!(old[0].run_id, "old-1");
    assert_eq!(old[0].kind.as_deref(), Some("sync"));
    assert_eq!(old[0].reason.as_deref(), Some("a sync asked for by ui: a"));

    let snap = snapshot_of(td.path(), Some("old-1")).await.unwrap();
    assert!(snap.old_run);
    assert_eq!(snap.run_id.as_deref(), Some("old-1"));
    let steps: Vec<(&str, &str, i64)> = snap
        .steps
        .iter()
        .map(|s| (s.step.as_str(), s.state.as_str(), s.attempt))
        .collect();
    assert_eq!(steps, [("a", "succeeded", 1), ("b", "failed", 2)]);
    assert_eq!(snap.errors.get("b"), Some(&1));
    assert_eq!(
        datalib_runs::step_samples(td.path(), "old-1", "a")
            .await
            .unwrap()
            .len(),
        1
    );

    let log = log_after(td.path(), "old-1", None, 0, 10).await.unwrap();
    assert_eq!(
        log.iter().map(|l| l.msg.as_str()).collect::<Vec<_>>(),
        ["boom"]
    );
    let by_term = log_query(
        td.path(),
        &LogQuery {
            q: "run:old-1",
            cursor: LogCursor::After(0),
            limit: 10,
        },
    )
    .await
    .unwrap();
    assert_eq!(by_term.len(), 1);
    assert_eq!(by_term[0].run_id.as_deref(), Some("old-1"));
    assert_eq!(
        processes(td.path(), Some("old-1"), None, 10)
            .await
            .unwrap()
            .len(),
        2
    );

    let latest = datalib_runs::latest_metric(td.path(), "problems")
        .await
        .unwrap();
    let of = |step: &str| {
        latest
            .iter()
            .find(|m| m.step == step)
            .map(|m| (m.value, m.reported_in.to_string()))
    };
    assert_eq!(of("a"), Some((1, "invocation r-new/a".to_string())));
    assert_eq!(of("b"), Some((3, "run old-1".to_string())));
}

/// A store from version 10 is upgraded in place, not remade: its runs and
/// their lines are still there, nothing is set aside, and its processes
/// take the columns later versions added. Without the rungs the old run's
/// line was gone and a `runs.bak_` copy sat beside it.
#[tokio::test]
async fn a_store_from_version_10_is_upgraded_in_place() {
    let td = tempfile::tempdir().unwrap();
    datalib_runs::prepare_store(td.path()).await.unwrap();
    let path = datalib_runs::runs_path(td.path());
    let pool = datalib_runs::open_or_create(&path).await.unwrap();
    Sql::dynamic(test_sql_id!(), BACK_TO_15)
        .raw()
        .execute(&pool)
        .await
        .unwrap();
    // Version 10's shape is 15's without what 11 through 15 added.
    test_sql!(
        "DROP INDEX metric_samples_by_name; \
         DROP INDEX metrics_by_name; \
         DROP INDEX runs_by_start; \
         DROP INDEX processes_by_run; \
         ALTER TABLE processes DROP COLUMN peak_rss_bytes; \
         ALTER TABLE processes DROP COLUMN applet; \
         ALTER TABLE runs DROP COLUMN kind; \
         ALTER TABLE runs DROP COLUMN reason; \
         INSERT INTO runs (run_id, started_at_utc, tz_offset) \
           VALUES ('run-1', '2026-11-01T10:00:00.000000+00:00', '+00:00'); \
         INSERT INTO processes (process_id, process, run_id, started_at_utc, tz_offset) \
           VALUES ('dag-1', 'dag', 'run-1', '2026-11-01T10:00:00.000000+00:00', '+00:00'); \
         INSERT INTO log (run_id, process_id, step, attempt, ts_utc, level, msg) \
           VALUES ('run-1', 'dag-1', 'a', 1, '2026-11-01T10:00:01.000000+00:00', 'info', \
           'from before'); \
         PRAGMA user_version = 10"
    )
    .raw()
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let server = ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default())
        .expect("the upgraded store opens");
    server.process(ProcessRow {
        process_id: "applet-1".into(),
        process: Process::Applet.as_str().into(),
        applet: Some("search".into()),
        started_at_utc: T0.into(),
        peak_rss_bytes: Some(4096),
        ..Default::default()
    });
    server.flush().await;
    drop(server);

    assert_eq!(
        log_after(td.path(), "run-1", None, 0, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let applet = datalib_runs::process(td.path(), "applet-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (applet.applet.as_deref(), applet.peak_rss_bytes),
        (Some("search"), Some(4096))
    );
    let dir = path.parent().unwrap();
    let set_aside = std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("runs.bak_")
        })
        .count();
    assert_eq!(set_aside, 0, "nothing was set aside");
    let pool = datalib_runs::open_or_create(&path).await.unwrap();
    let index: i64 = test_sql!(
        "SELECT count(*) FROM sqlite_master WHERE type = 'index' \
         AND name IN ('processes_by_run', 'runs_by_start', 'metrics_by_name', \
           'metric_samples_by_name', 'log_by_invocation', 'invocations_by_step')"
    )
    .query_scalar()
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(
        index, 6,
        "the climb adds the indexes retention and the metric reads name"
    );
}

/// A process another one reports is written again as its peak rises and
/// once more as it ends; the last row need not repeat the peak for the
/// store to keep it.
#[tokio::test]
async fn a_process_keeps_its_peak_when_its_last_row_names_none() {
    let td = tempfile::tempdir().unwrap();
    let server = ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default())
        .expect("start the store");
    let row = ProcessRow {
        process_id: "applet-1".into(),
        process: Process::Applet.as_str().into(),
        applet: Some("search".into()),
        started_at_utc: T0.into(),
        ..Default::default()
    };
    server.process(ProcessRow {
        peak_rss_bytes: Some(4096),
        ..row.clone()
    });
    server.flush().await;
    server.process(ProcessRow {
        finished_at_utc: Some(T0.into()),
        signal: Some(9),
        ..row
    });
    server.flush().await;

    let stored = datalib_runs::process(td.path(), "applet-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.peak_rss_bytes, Some(4096));
    assert_eq!(stored.signal, Some(9));
}

/// A chart over an invocation needs its ends. The writer samples a series
/// when its value changed and the floor between samples has passed, and
/// once more when the invocation ends or the writer closes — so a series
/// that moved twice inside the floor still leaves its first and last
/// values. The snapshot says when each step last logged.
#[tokio::test]
async fn a_series_keeps_its_first_and_last_values_and_the_snapshot_its_last_log_time() {
    let td = tempfile::tempdir().unwrap();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let recent = |secs_ago: i64| now.bump_micros(-secs_ago * 1_000_000).to_utc_and_offset().0;
    {
        let w = start(td.path(), "r-1");
        w.metric(InvocationMetricRow {
            updated_at_utc: recent(30),
            ..metric("a", "rows", 1)
        });
        w.log(line("a", "info", "first"));
        // The first value has to reach the store before the second is
        // published, or the two coalesce in the writer's batch and the
        // series gets one sample.
        w.flush().await;
        // Inside the sample floor: the second value is the last, which
        // the invocation's end samples.
        w.metric(InvocationMetricRow {
            updated_at_utc: recent(20),
            ..metric("a", "rows", 7)
        });
        w.log(LogRow {
            ts_utc: "2026-09-14T10:00:03.500000+00:00".into(),
            ..line("a", "info", "second")
        });
        w.step(at("a", "succeeded", "done"));
        w.flush().await;
        let samples = datalib_runs::step_samples(td.path(), "r-1", "a")
            .await
            .unwrap();
        let values: Vec<i64> = samples.iter().map(|s| s.value).collect();
        assert_eq!(
            values,
            [1, 7],
            "the end, not the writer's close: {samples:?}"
        );
    }
    let snap = snapshot(td.path()).await.unwrap();
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
    let w = start(td.path(), "r-1");

    let mut seen = std::collections::BTreeSet::new();
    for i in 0..40 {
        w.metric(metric("slack/raw", "done_total", i));
        w.flush().await;
        for m in snapshot(td.path()).await.unwrap().metrics {
            seen.insert(m.value);
        }
    }
    drop(w);
    assert!(
        seen.len() > 1,
        "a reader polling during a sync must see progress move, saw {seen:?}"
    );
}

/// What a writer holds is readable the moment `flush` returns, not a
/// timer tick later.
#[tokio::test]
async fn a_flushed_line_is_readable_at_once() {
    let td = tempfile::tempdir().unwrap();
    let server =
        ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap();
    for msg in ["one", "two"] {
        server.log(LogRow {
            ts_utc: T0.into(),
            level: "info".into(),
            msg: msg.into(),
            ..Default::default()
        });
        server.flush().await;
        let all = log_query(
            td.path(),
            &LogQuery {
                q: "",
                cursor: LogCursor::After(0),
                limit: 100,
            },
        )
        .await
        .unwrap();
        assert_eq!(all.last().map(|l| l.msg.as_str()), Some(msg));
    }
    let w = start(td.path(), "r-1");
    w.metric(metric("a", "rows", 3));
    w.flush().await;
    let snap = snapshot(td.path()).await.unwrap();
    assert_eq!(
        snap.metrics.iter().map(|m| m.value).collect::<Vec<_>>(),
        [3]
    );
}

/// The server's lines share the table with the invocations': about no
/// invocation, from the process that wrote them, on the same tail cursor.
/// Both writers hold the file at once, which on plain SQLite is ordinary,
/// and retention of invocations leaves the server's lines alone.
#[tokio::test]
async fn a_process_log_sits_beside_the_invocations_and_survives_them() {
    let td = tempfile::tempdir().unwrap();
    // The rows below are stamped on a fixed date; only `max_runs` is
    // under test, so neither age limit may reach them.
    let keep = Retention {
        max_runs: 1,
        ..KEEP
    };
    let server =
        ProcessLogWriter::start(td.path(), Process::Http, Some("ae2d52f0".into()), keep).unwrap();
    server.log(LogRow {
        ts_utc: "2026-09-15T10:00:00.000000+00:00".into(),
        level: "info".into(),
        target: Some("datalib_http::worker".into()),
        msg: "ready".into(),
        ..Default::default()
    });
    // The line has to be in the file before the invocations' are, or
    // `seq` does not read in the order things happened.
    server.flush().await;
    {
        let w = Request::dated(td.path(), "r-1", "2026-09-15T10:00:01.000000+00:00", keep);
        w.step(at("a", "succeeded", ""));
        w.log(line("a", "warn", "from request 1"));
    }
    // A second request under `max_runs: 1` sweeps r-1's rows at the next
    // open; the server's must stay, since they belong to no invocation.
    let runner = {
        let w = Request::with(
            td.path(),
            "r-2",
            "2026-09-15T10:00:02.000000+00:00",
            Some("f2068bc9".into()),
            keep,
        );
        w.step(at("a", "succeeded", ""));
        w.log(line("a", "info", "from request 2"));
        w.w.process_id().to_string()
    };
    apply_retention(td.path(), keep);
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
            q: "",
            cursor: LogCursor::After(0),
            limit: 100,
        },
    )
    .await
    .unwrap();
    // Every line reads as its process says: which program wrote it and
    // from which commit — the server's for the server's lines, the
    // runner's for what it filed under an invocation.
    let seen: Vec<String> = all
        .iter()
        .map(|l| {
            format!(
                "{} {} {} {}",
                l.invocation_id.as_deref().unwrap_or("-"),
                l.process.as_deref().unwrap_or("-"),
                l.msg,
                l.git_hash.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            "- http ready ae2d52f0",
            "r-2/a dag from request 2 f2068bc9",
            "- http still here ae2d52f0",
        ]
    );
    let ids: Vec<&str> = all.iter().map(|l| l.process_id.as_str()).collect();
    assert_eq!(ids[0], ids[2]);
    assert_eq!(ids[1], runner);
    let runner = processes(td.path(), None, None, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.process_id == runner)
        .unwrap();
    assert_eq!((runner.run_id, runner.invocation_id), (None, None));
    assert!(
        runner.finished_at_utc.is_some(),
        "the writer closing is the process ending"
    );

    let servers_only = log_query(
        td.path(),
        &LogQuery {
            q: "process:http",
            cursor: LogCursor::After(0),
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(servers_only.len(), 2);
    assert!(servers_only.iter().all(|l| l.invocation_id.is_none()));

    // `min_level:` is this level and above, across every writer.
    let loud = log_query(
        td.path(),
        &LogQuery {
            q: "min_level:warn",
            cursor: LogCursor::After(0),
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        loud.iter().map(|l| l.msg.as_str()).collect::<Vec<_>>(),
        ["still here"]
    );

    // `commit:` finds the lines of a build by a prefix of its commit.
    let one_build = log_query(
        td.path(),
        &LogQuery {
            q: "commit:f2068",
            cursor: LogCursor::After(0),
            limit: 100,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        one_build.iter().map(|l| l.msg.as_str()).collect::<Vec<_>>(),
        ["from request 2"]
    );
}

/// A line about no invocation has its own age limit, shorter than an
/// invocation's, and both writers apply it when they open.
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
        let server = ProcessLogWriter::start(td.path(), Process::Http, None, keep).unwrap();
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
    apply_retention(td.path(), keep);
    let all = log_query(
        td.path(),
        &LogQuery {
            q: "process:http",
            cursor: LogCursor::After(0),
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

/// A process row goes when nothing names it any more — no invocation, no
/// line — and it is past the process-log age; never before, since the
/// process may still be alive and about to write.
#[tokio::test]
async fn a_process_is_pruned_only_once_nothing_names_it() {
    let td = tempfile::tempdir().unwrap();
    let keep = Retention {
        process_log_days: 3,
        ..Retention::default()
    };
    {
        let server = ProcessLogWriter::start(td.path(), Process::Http, None, keep).unwrap();
        server.log(LogRow {
            ts_utc: "2020-01-01T00:00:00.000000+00:00".into(),
            level: "info".into(),
            msg: "ancient".into(),
            ..Default::default()
        });
        server.flush().await;
    }
    let pool = datalib_runs::open_or_create(&datalib_runs::runs_path(td.path()))
        .await
        .unwrap();
    // A launch from long ago whose every line has already aged out.
    test_sql!(
        "INSERT INTO processes (process_id, process, started_at_utc, tz_offset, git_hash) \
         VALUES ('old', 'http', '2020-01-01T00:00:00.000000+00:00', '+00:00', NULL)"
    )
    .query()
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    // The next writer applies retention: the ancient line goes, and
    // with it nothing names `old`; the first server's row started just
    // now and stays, lines or no lines.
    drop(ProcessLogWriter::start(td.path(), Process::Http, None, keep).unwrap());
    let pool = datalib_runs::open_or_create(&datalib_runs::runs_path(td.path()))
        .await
        .unwrap();
    let left: Vec<(String, String)> =
        test_sql!("SELECT process_id, started_at_utc FROM processes ORDER BY started_at_utc")
            .query_as()
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.iter().all(|(id, _)| id != "old"), "{left:?}");
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
        let server = ProcessLogWriter::start(td.path(), Process::Http, None, keep).unwrap();
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
    drop(ProcessLogWriter::start(td.path(), Process::Http, None, keep).unwrap());
    let all = log_query(
        td.path(),
        &LogQuery {
            q: "process:http",
            cursor: LogCursor::After(0),
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

/// The two retention passes over the lines about no invocation read every
/// one they keep, so each open of a root with many warned of a heavy
/// statement.
#[tokio::test]
async fn retention_of_the_lines_about_no_invocation_is_marked_as_walking_by_design() {
    let td = tempfile::tempdir().unwrap();
    let recording = datalib_sql_work::record_under(td.path());
    drop(ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap());
    let prunes: Vec<String> = recording
        .take()
        .into_iter()
        .map(|ran| ran.sql)
        .filter(|sql| {
            datalib_sql::tag::after_name(sql)
                .trim_start()
                .starts_with("DELETE FROM log")
                && sql.contains("invocation_id IS NULL")
        })
        .collect();
    assert_eq!(prunes.len(), 2, "{prunes:#?}");
    for sql in prunes {
        assert!(datalib_sql_work::judge::walks_by_design(&sql), "{sql}");
    }
}

/// Every write counts itself under the part of the store it touched,
/// and an invocation's log lines count apart from the server's — that is
/// what lets a watcher wake the Manage screen for one and not the other.
#[tokio::test]
async fn each_part_of_the_store_counts_its_own_writes() {
    let td = tempfile::tempdir().unwrap();
    assert!(
        versions(td.path()).await.unwrap().is_empty(),
        "no store, no versions"
    );

    {
        let w = start(td.path(), "r-1");
        w.step(at("a", "running", ""));
        w.log(LogRow {
            step: Some("a".into()),
            level: "info".into(),
            msg: "hello".into(),
            ..Default::default()
        });
        w.metric(metric("a", "rows", 1));
    }
    let after_run = versions(td.path()).await.unwrap();
    let parts = [
        StorePart::Invocations,
        StorePart::Metrics,
        StorePart::InvocationLog,
    ];
    for part in parts.iter().chain([&StorePart::Processes]) {
        assert!(
            after_run.get(part).is_some_and(|v| *v > 0),
            "{part:?}: {after_run:?}"
        );
    }
    assert_eq!(after_run.get(&StorePart::ProcessLog), None);

    {
        let server =
            ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap();
        server.log(LogRow {
            level: "debug".into(),
            msg: "served".into(),
            ..Default::default()
        });
    }
    let after_server = versions(td.path()).await.unwrap();
    assert!(after_server
        .get(&StorePart::ProcessLog)
        .is_some_and(|v| *v > 0));
    for part in parts {
        assert_eq!(
            after_server.get(&part),
            after_run.get(&part),
            "{part:?} moved for a server line"
        );
    }
}

/// An invocation's progress message is written about once a second while
/// it runs. The log panel's process picker refetches on `processes`, so a
/// progress tick must leave that part alone and a new attempt must move
/// it.
#[tokio::test]
async fn a_progress_tick_moves_the_invocations_and_not_the_processes() {
    let td = tempfile::tempdir().unwrap();
    let w = start(td.path(), "r-1");
    let attempt = |n: i64| ProcessRow {
        process_id: format!("step-a-{n}"),
        process: Process::Step.as_str().into(),
        step: Some("a".into()),
        attempt: Some(n),
        started_at_utc: T0.into(),
        ..Default::default()
    };
    w.process(attempt(1));
    w.step(at("a", "running", "loaded 0/10"));
    w.flush().await;
    let before = versions(td.path()).await.unwrap();

    w.step(at("a", "running", "loaded 5/10"));
    w.flush().await;
    let ticked = versions(td.path()).await.unwrap();
    assert!(ticked[&StorePart::Invocations] > before[&StorePart::Invocations]);
    assert_eq!(
        ticked.get(&StorePart::Processes),
        before.get(&StorePart::Processes),
        "a progress tick moved the processes"
    );

    w.process(attempt(2));
    w.flush().await;
    let retried = versions(td.path()).await.unwrap();
    assert!(
        retried[&StorePart::Processes] > ticked[&StorePart::Processes],
        "a new attempt did not move the processes: {retried:?}"
    );
}

/// The server hears its own writes from this hook rather than waiting on
/// the filesystem's report, which fseventsd can hold back for seconds.
#[tokio::test]
async fn every_write_is_told_to_the_hooks_with_its_store() {
    let td = tempfile::tempdir().unwrap();
    let store = datalib_runs::runs_path(td.path());
    let heard = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        // Other tests' writers share the process, and so the hook.
        let (store, heard) = (store.clone(), heard.clone());
        datalib_runs::after_every_write(move |written| {
            if written == store {
                heard.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }
    let server = ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default())
        .expect("start the store");
    // The start is recorded once the store is open, after `start` has
    // returned; a flush waits for it.
    server.flush().await;
    let started = heard.load(std::sync::atomic::Ordering::SeqCst);
    assert!(started >= 1, "recording the process's start was not told");
    server.log(LogRow {
        level: "info".into(),
        msg: "a line".into(),
        ..Default::default()
    });
    server.flush().await;
    assert!(
        heard.load(std::sync::atomic::Ordering::SeqCst) > started,
        "a flushed line was not told"
    );
}
