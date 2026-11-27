//! `/api/runs` lists what the app calls runs — each request and job, and
//! each run an older build recorded — and reads the run store the runner
//! wrote for one: its invocations, its steps with their numbers, and a
//! tail of its log.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_dag::supervisor::store::{RequestOutcome, Store};
use datalib_http::router;
use datalib_runs::{InvocationMetricRow, InvocationRow, LogRow, LoopWriter, Retention};
use datalib_sql::test_sql;
use std::path::Path;
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

async fn get(root: &Path, uri: &str) -> serde_json::Value {
    let app = router(state(root).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Open (and, when `closed`, close) a request under `id`, as a client
/// and the loop would.
async fn request(root: &Path, id: &str, roots: &[&str], closed: bool) {
    let store = Store::open(root).await.unwrap();
    let roots: Vec<String> = roots.iter().map(|r| r.to_string()).collect();
    store.open_request_as(Some(id), &roots, "ui").await.unwrap();
    if closed {
        store
            .close_request(id, RequestOutcome::Done, None)
            .await
            .unwrap();
    }
    store.close().await;
}

fn writer(root: &Path) -> LoopWriter {
    let keep = Retention {
        max_runs: 100,
        max_age_days: 36500,
        ..Retention::default()
    };
    LoopWriter::start(root, None, keep).unwrap()
}

fn invocation(id: &str, step: &str, state: &str, t: &str) -> InvocationRow {
    InvocationRow {
        invocation_id: id.into(),
        step: step.into(),
        state: state.into(),
        attempt: 1,
        started_at_utc: t.into(),
        updated_at_utc: t.into(),
        ..Default::default()
    }
}

/// Two syncs of Slack: `run-1` closed, its one invocation of the ingest
/// done; `run-2` open, its ingest running and its render blocked.
async fn write_two_runs(root: &Path) {
    request(root, "run-1", &["slack/ingest"], true).await;
    request(root, "run-2", &["slack/ingest"], false).await;
    let t = "2026-09-11T10:00:00+01:00";
    let w = writer(root);
    w.invocation(invocation("i-1", "slack/ingest", "succeeded", t));
    w.served("i-1", &["run-1".to_string()]);
    w.log(LogRow {
        invocation_id: Some("i-1".into()),
        step: Some("slack/ingest".into()),
        ts_utc: t.into(),
        level: "error".into(),
        msg: "from run 1".into(),
        ..Default::default()
    });
    w.invocation(InvocationRow {
        msg: Some("conversations.list".into()),
        ..invocation("i-2", "slack/ingest", "running", t)
    });
    w.served("i-2", &["run-2".to_string()]);
    w.invocation(invocation("i-3", "slack/render_markdown", "blocked", t));
    w.served("i-3", &["run-2".to_string()]);
    for (name, labels, value) in [
        ("rows_upserted_total", "table=slack_messages", 42),
        ("queued", "", 7),
    ] {
        w.metric(InvocationMetricRow {
            invocation_id: "i-2".into(),
            step: "slack/ingest".into(),
            name: name.into(),
            labels: labels.into(),
            value,
            updated_at_utc: t.into(),
            tz_offset: None,
        });
    }
    for (level, msg) in [("info", "hello"), ("warn", "slow"), ("info", "still here")] {
        w.log(LogRow {
            invocation_id: Some("i-2".into()),
            step: Some("slack/ingest".into()),
            attempt: 1,
            ts_utc: t.into(),
            stream: Some("stderr".into()),
            level: level.into(),
            thread: Some("main".into()),
            msg: msg.into(),
            ..Default::default()
        });
    }
}

#[tokio::test]
async fn runs_are_listed_newest_first_and_filtered_by_step() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path()).await;

    let all = get(td.path(), "/api/runs").await;
    let ids: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["run-2", "run-1"]);

    let with = get(td.path(), "/api/runs?step=slack/render_markdown").await;
    assert_eq!(with.as_array().unwrap().len(), 1);
    assert_eq!(with[0]["run_id"], "run-2");

    assert!(get(td.path(), "/api/runs?step=nope")
        .await
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_runs_steps_carry_their_numbers_and_error_counts() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path()).await;

    let v = get(td.path(), "/api/runs/run-2/steps").await;
    assert_eq!(v["run"]["run_id"], "run-2");
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    let ingest = steps.iter().find(|s| s["step"] == "slack/ingest").unwrap();
    assert_eq!(ingest["state"], "running");
    assert_eq!(ingest["progress"]["msg"], "conversations.list");
    assert_eq!(
        ingest["progress"]["metrics"]["rows_upserted_total{table=slack_messages}"],
        42
    );
    assert_eq!(ingest["progress"]["metrics"]["queued"], 7);
    assert_eq!(ingest["progress"]["errors"], 1, "one warn, no errors");

    let missing = get(td.path(), "/api/runs/run-9/steps").await;
    assert!(missing["run"].is_null());
    assert!(missing["steps"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_runs_log_is_tailed_by_seq_and_narrowed_by_step() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path()).await;

    let first = get(td.path(), "/api/runs/run-2/log?step=slack/ingest&limit=2").await;
    let first = first.as_array().unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["msg"], "hello");
    assert_eq!(first[0]["stream"], "stderr");
    assert_eq!(first[0]["thread"], "main");
    assert_eq!(first[0]["attempt"], 1);

    let after = first[1]["seq"].as_i64().unwrap();
    let rest = get(
        td.path(),
        &format!("/api/runs/run-2/log?step=slack/ingest&after_seq={after}"),
    )
    .await;
    let rest = rest.as_array().unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["msg"], "still here");

    // The other run's lines are its own.
    let old = get(td.path(), "/api/runs/run-1/log").await;
    assert_eq!(old.as_array().unwrap().len(), 1);
    assert_eq!(old[0]["level"], "error");
}

/// `/api/log` reads the search-bar grammar: a term narrows by column, a
/// negated one drops, free text is a substring, and a key a log line
/// does not have is refused by name rather than matched against nothing.
#[tokio::test]
async fn the_log_is_read_through_the_shared_query_grammar() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path()).await;

    let all = get(td.path(), "/api/log?q=step:slack/ingest").await;
    assert_eq!(all.as_array().unwrap().len(), 4, "both runs' lines");

    let q = |q: &str| {
        format!(
            "/api/log?q={}",
            urlencoding(&format!("step:slack/ingest {q}"))
        )
    };
    let warned = get(td.path(), &q("level:warn")).await;
    assert_eq!(warned.as_array().unwrap().len(), 1);
    assert_eq!(warned[0]["msg"], "slow");

    let not_first = get(td.path(), &q("-run:run-1 -level:warn")).await;
    let msgs: Vec<&str> = not_first
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["msg"].as_str().unwrap())
        .collect();
    assert_eq!(msgs, ["hello", "still here"]);

    let phrase = get(td.path(), &q("\"still h\"")).await;
    assert_eq!(phrase.as_array().unwrap().len(), 1);

    let app = router(state(td.path()).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(q("author:thad"))
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("`author:`"), "{text}");
}

/// With no cursor the log answers with its newest lines; `before_seq`
/// pages back from the oldest one a panel holds, `after_seq` follows the
/// tail, and both at once is refused rather than guessed at.
#[tokio::test]
async fn the_log_opens_on_its_newest_lines_and_pages_back() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path()).await;
    let msgs = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|l| l["msg"].as_str().unwrap().to_string())
            .collect()
    };
    let all = get(td.path(), "/api/log?q=step:slack/ingest").await;
    let newest = get(td.path(), "/api/log?q=step:slack/ingest&limit=2").await;
    assert_eq!(msgs(&newest), msgs(&all)[2..]);
    let oldest_held = newest[0]["seq"].as_i64().unwrap();
    let before = get(
        td.path(),
        &format!("/api/log?q=step:slack/ingest&limit=2&before_seq={oldest_held}"),
    )
    .await;
    assert_eq!(msgs(&before), msgs(&all)[..2]);
    let after = get(
        td.path(),
        &format!("/api/log?q=step:slack/ingest&after_seq={oldest_held}"),
    )
    .await;
    assert_eq!(msgs(&after), msgs(&all)[3..]);

    // The panel's pickers are search terms now; a caller still sending
    // one as a parameter is refused rather than handed every line.
    let app = router(state(td.path()).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/log?step=slack/ingest")
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let app = router(state(td.path()).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/log?after_seq=1&before_seq=9")
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The inspector reads one line by its `seq`, with the process that
/// wrote it beside it; a `seq` the store no longer has is a 404.
#[tokio::test]
async fn one_line_is_read_by_seq_with_its_process() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path()).await;

    let warned = get(td.path(), "/api/log?q=level:warn").await;
    let seq = warned[0]["seq"].as_i64().unwrap();
    let one = get(td.path(), &format!("/api/log/{seq}")).await;
    assert_eq!(one["line"]["msg"], "slow");
    assert_eq!(one["line"]["invocation_id"], "i-2");
    assert_eq!(one["process"]["process"], "dag");
    assert_eq!(one["process"]["process_id"], one["line"]["process_id"]);

    let app = router(state(td.path()).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/log/999999")
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A run is a request: its invocations are listed under it, one that
/// served two requests under both, and its log is their lines and not
/// another request's. Before invocations a run was a busy period of the
/// loop, which neither `/api/runs` nor its log could tell a request
/// from. A run an older build recorded still lists, as old, with its
/// steps and its lines.
#[tokio::test]
async fn a_run_is_a_request_with_its_invocations_and_an_old_run_stays_readable() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    request(root, "req-a", &["slack/ingest"], true).await;
    request(root, "req-b", &["slack/render_markdown"], true).await;
    request(root, "req-c", &["gmail/ingest"], false).await;
    let t = "2026-09-11T10:00:00+01:00";
    {
        let w = writer(root);
        for (id, step, served) in [
            ("x", "slack/ingest", &["req-a"][..]),
            ("y", "slack/render_markdown", &["req-a", "req-b"][..]),
            ("z", "gmail/ingest", &["req-c"][..]),
        ] {
            w.invocation(invocation(id, step, "succeeded", t));
            w.served(
                id,
                &served.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            );
            w.log(LogRow {
                invocation_id: Some(id.into()),
                step: Some(step.into()),
                ts_utc: t.into(),
                level: "info".into(),
                msg: format!("said by {id}"),
                ..Default::default()
            });
        }
    }
    // A run as a build before invocations wrote it: its rows carry its id.
    let pool = datalib_runs::open_or_create(&datalib_runs::runs_path(root))
        .await
        .unwrap();
    for statement in [
        test_sql!(
            "INSERT INTO runs (run_id, started_at_utc, finished_at_utc, tz_offset, kind, reason) \
             VALUES ('old-1', '2026-09-01T10:00:00.000000+00:00', \
             '2026-09-01T10:05:00.000000+00:00', '+00:00', 'sync', 'a sync asked for by ui')"
        ),
        test_sql!(
            "INSERT INTO step_runs (run_id, step, state, attempt, updated_at_utc) \
             VALUES ('old-1', 'slack/ingest', 'succeeded', 1, '2026-09-01T10:05:00.000000+00:00')"
        ),
        test_sql!(
            "INSERT INTO log (run_id, process_id, step, attempt, ts_utc, level, msg) \
             VALUES ('old-1', 'p-old', 'slack/ingest', 1, '2026-09-01T10:01:00.000000+00:00', \
             'info', 'said in the old run')"
        ),
    ] {
        statement.query().execute(&pool).await.unwrap();
    }
    pool.close().await;

    let runs = get(root, "/api/runs").await;
    let listed: Vec<(&str, bool)> = runs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["run_id"].as_str().unwrap(), r["old"].as_bool().unwrap()))
        .collect();
    for want in [
        ("req-a", false),
        ("req-b", false),
        ("req-c", false),
        ("old-1", true),
    ] {
        assert!(listed.contains(&want), "{want:?} in {listed:?}");
    }

    let ids = |v: serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|i| i["invocation_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(get(root, "/api/runs/req-a/invocations").await),
        ["x", "y"]
    );
    assert_eq!(ids(get(root, "/api/runs/req-b/invocations").await), ["y"]);
    assert!(ids(get(root, "/api/runs/old-1/invocations").await).is_empty());

    let msgs = |v: serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|l| l["msg"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        msgs(get(root, "/api/runs/req-a/log").await),
        ["said by x", "said by y"]
    );
    assert_eq!(
        msgs(get(root, "/api/runs/old-1/log").await),
        ["said in the old run"]
    );
    let not_c = get(root, &format!("/api/log?q={}", urlencoding("-run:req-c"))).await;
    assert!(
        !msgs(not_c.clone()).contains(&"said by z".to_string()),
        "{not_c}"
    );
    assert_eq!(msgs(not_c).len(), 3, "a negated run keeps every other line");

    let old = get(root, "/api/runs/old-1/steps").await;
    assert_eq!(old["run"]["old"], true);
    assert_eq!(old["steps"][0]["step"], "slack/ingest");
    assert_eq!(old["steps"][0]["state"], "succeeded");
    let a = get(root, "/api/runs/req-a/steps").await;
    assert_eq!(a["run"]["kind"], "sync");
    assert_eq!(a["run"]["outcome"], "done");
    assert_eq!(a["steps"].as_array().unwrap().len(), 2);
}
