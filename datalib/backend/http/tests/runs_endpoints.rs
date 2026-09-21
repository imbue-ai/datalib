//! `/api/runs` reads the run store the runner wrote: recent runs, one
//! run's steps with their numbers, and a tail of its log.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use datalib_runs::{LogRow, MetricRow, Retention, RunWriter, StepRunRow};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "runs-endpoints-test-token";

async fn state(root: &Path) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        usage: Default::default(),
        newer_root: Vec::new(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

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

fn write_two_runs(root: &Path) {
    let keep = Retention {
        max_runs: 100,
        max_age_days: 36500,
        ..Retention::default()
    };
    let t = "2026-09-11T10:00:00+01:00";
    {
        let w = RunWriter::start(root, "run-1", "2026-09-10T10:00:00+01:00", None, keep).unwrap();
        w.step(StepRunRow {
            step: "slack/ingest".into(),
            state: "succeeded".into(),
            attempt: 1,
            updated_at_utc: t.into(),
            ..Default::default()
        });
        w.log(LogRow {
            step: Some("slack/ingest".into()),
            ts_utc: t.into(),
            level: "error".into(),
            msg: "from run 1".into(),
            ..Default::default()
        });
    }
    {
        let w = RunWriter::start(root, "run-2", "2026-09-11T10:00:00+01:00", None, keep).unwrap();
        w.step(StepRunRow {
            step: "slack/ingest".into(),
            state: "running".into(),
            attempt: 1,
            msg: Some("conversations.list".into()),
            updated_at_utc: t.into(),
            ..Default::default()
        });
        w.step(StepRunRow {
            step: "slack/render_markdown".into(),
            state: "pending".into(),
            updated_at_utc: t.into(),
            ..Default::default()
        });
        w.metric(MetricRow {
            step: "slack/ingest".into(),
            name: "rows_upserted".into(),
            labels: "table=slack_messages".into(),
            value: 42,
            updated_at_utc: t.into(),
            ..Default::default()
        });
        w.metric(MetricRow {
            step: "slack/ingest".into(),
            name: "queued".into(),
            labels: String::new(),
            value: 7,
            updated_at_utc: t.into(),
            ..Default::default()
        });
        for (level, msg) in [("info", "hello"), ("warn", "slow"), ("info", "still here")] {
            w.log(LogRow {
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
}

#[tokio::test]
async fn runs_are_listed_newest_first_and_filtered_by_step() {
    let td = tempfile::tempdir().unwrap();
    write_two_runs(td.path());

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
    write_two_runs(td.path());

    let v = get(td.path(), "/api/runs/run-2/steps").await;
    assert_eq!(v["run"]["run_id"], "run-2");
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    let ingest = steps.iter().find(|s| s["step"] == "slack/ingest").unwrap();
    assert_eq!(ingest["state"], "running");
    assert_eq!(ingest["progress"]["msg"], "conversations.list");
    assert_eq!(
        ingest["progress"]["metrics"]["rows_upserted{table=slack_messages}"],
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
    write_two_runs(td.path());

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
    write_two_runs(td.path());

    let all = get(td.path(), "/api/log?step=slack/ingest").await;
    assert_eq!(all.as_array().unwrap().len(), 4, "both runs' lines");

    let q = |q: &str| format!("/api/log?step=slack/ingest&q={}", urlencoding(q));
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
    write_two_runs(td.path());

    let warned = get(td.path(), "/api/log?q=level:warn").await;
    let seq = warned[0]["seq"].as_i64().unwrap();
    let one = get(td.path(), &format!("/api/log/{seq}")).await;
    assert_eq!(one["line"]["msg"], "slow");
    assert_eq!(one["line"]["run_id"], "run-2");
    assert_eq!(one["process"]["process"], "dag");
    assert_eq!(one["process"]["run_id"], "run-2");
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
