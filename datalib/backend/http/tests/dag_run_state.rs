//! `GET /api/dag` carries the runner's own per-step run record.
//!
//! Why the endpoint and not the job queue: `sync_jobs` records whole
//! *runs*, and a run routinely names several steps, so the table could
//! only ever attribute one timestamp and one status to all of them —
//! which is what the `~` marker beside "Last status" used to be
//! apologizing for. The runner knows per step, writes it to
//! `system/dag_state.json`, and this is how that reaches the UI.
//!
//! The other half of the point is that the *runner* writes it. A sync
//! started from a terminal leaves the same record as one the app kicked
//! off, so both show up in the table. That is not directly assertable
//! here — it follows from reading the file rather than the queue, which
//! is what these tests pin.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "dag-run-state-test-token";

async fn state(root: &Path) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        // No sampler running here, so the monitor is empty and every
        // tree reports as absent — the state a root nobody has walked
        // is in.
        usage: Default::default(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn get_dag(root: &Path) -> serde_json::Value {
    let app = router(state(root).await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/dag")
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

const CONFIG: &str = r#"
[[steps]]
id = "slack/raw"
command = "datalib-step download slack_api"

[[steps]]
id = "slack/rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
"#;

fn write_root(root: &Path, state_json: Option<&str>) {
    std::fs::create_dir_all(root.join("system")).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    if let Some(j) = state_json {
        std::fs::write(root.join("system/dag_state.json"), j).unwrap();
    }
}

/// A root that has never synced: every step reports no last run, and
/// there is no run to report. "Never run" has to be distinguishable
/// from "ran and we lost the record".
#[tokio::test]
async fn a_root_that_never_ran_reports_no_history() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(tmp.path(), None);

    let dag = get_dag(tmp.path()).await;
    assert_eq!(dag["ok"], true, "{dag}");
    assert_eq!(dag["run"], serde_json::Value::Null);
    assert_eq!(dag["steps"].as_array().unwrap().len(), 2);
    for step in dag["steps"].as_array().unwrap() {
        assert_eq!(step["last_run"], serde_json::Value::Null, "{step}");
        assert_eq!(step["current_state"], serde_json::Value::Null, "{step}");
    }
}

/// A finished run: each step carries its own timings and outcome, so
/// "last synced" is per step rather than a whole run's timestamp
/// smeared across everything it named.
#[tokio::test]
async fn a_finished_run_surfaces_per_step_outcomes() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(
        tmp.path(),
        Some(
            r#"{
              "steps": {
                "slack/raw": {
                  "succeeded": true,
                  "last_run": {
                    "started_at": "2026-08-31T10:00:00+01:00",
                    "finished_at": "2026-08-31T10:00:09+01:00",
                    "status": "succeeded",
                    "attempts": 1
                  }
                },
                "slack/rendered_md": {
                  "succeeded": false,
                  "last_run": {
                    "started_at": "2026-08-31T10:00:09+01:00",
                    "finished_at": "2026-08-31T10:00:11+01:00",
                    "status": "failed",
                    "attempts": 2,
                    "error": "bad json at line 3"
                  }
                }
              },
              "current_run": {
                "run_id": "2026-08-31T10:00:00+01:00",
                "started_at": "2026-08-31T10:00:00+01:00",
                "finished_at": "2026-08-31T10:00:12+01:00",
                "plan": ["slack/raw", "slack/rendered_md"],
                "states": {"slack/raw": "succeeded", "slack/rendered_md": "failed"}
              }
            }"#,
        ),
    );

    let dag = get_dag(tmp.path()).await;
    assert_eq!(dag["run"]["run_id"], "2026-08-31T10:00:00+01:00");
    assert_eq!(dag["run"]["finished_at"], "2026-08-31T10:00:12+01:00");
    assert_eq!(
        dag["run"]["live"], false,
        "a run with a finished_at is not live"
    );

    let by: std::collections::HashMap<&str, &serde_json::Value> = dag["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s))
        .collect();

    let fetch = by["slack/raw"];
    assert_eq!(fetch["last_run"]["status"], "succeeded");
    assert_eq!(
        fetch["last_run"]["finished_at"],
        "2026-08-31T10:00:09+01:00"
    );
    assert_eq!(fetch["current_state"], "succeeded");

    // The failure is legible without opening a log.
    let render = by["slack/rendered_md"];
    assert_eq!(render["last_run"]["status"], "failed");
    assert_eq!(render["last_run"]["attempts"], 2);
    assert_eq!(render["last_run"]["error"], "bad json at line 3");
}

/// A run record with no `finished_at` and nobody holding the runner
/// lock is a run that *died*. Reporting it as live would spin the UI
/// forever; the lock is what makes the difference knowable, since the
/// kernel drops it when the holder exits however it exits.
#[tokio::test]
async fn an_open_record_with_no_lock_holder_is_not_live() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(
        tmp.path(),
        Some(
            r#"{
              "steps": {},
              "current_run": {
                "run_id": "2026-08-31T10:00:00+01:00",
                "started_at": "2026-08-31T10:00:00+01:00",
                "plan": ["slack/raw"],
                "states": {"slack/raw": "running"}
              }
            }"#,
        ),
    );

    let dag = get_dag(tmp.path()).await;
    assert_eq!(dag["run"]["finished_at"], serde_json::Value::Null);
    assert_eq!(
        dag["run"]["live"], false,
        "no runner holds this root, so the open record is a crashed run"
    );
    // The step's own state is still reported — the UI decides what to
    // call it, and needs to know it was mid-flight when the run died.
    let step = &dag["steps"].as_array().unwrap()[0];
    assert_eq!(step["current_state"], "running");
}

/// ...and with a runner actually holding the root, the same record is
/// live. This is the pair that makes the previous test mean something.
#[tokio::test]
#[cfg(unix)]
async fn an_open_record_is_live_while_a_runner_holds_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(
        tmp.path(),
        Some(
            r#"{
              "steps": {},
              "current_run": {
                "run_id": "r", "started_at": "2026-08-31T10:00:00+01:00",
                "plan": ["slack/raw"], "states": {"slack/raw": "running"}
              }
            }"#,
        ),
    );

    let _held = datalib_dag::lock::FileLock::acquire_runner(tmp.path()).expect("take the lock");
    let dag = get_dag(tmp.path()).await;
    assert_eq!(dag["run"]["live"], true);
}
