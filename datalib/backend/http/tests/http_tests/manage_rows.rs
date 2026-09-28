//! `GET /api/manage/rows` assembles the Manage screen's tree: one row
//! per entry in the config file, with status read off the loop's
//! record.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_http::{router, AppState};
use std::collections::HashMap;
use std::path::Path;
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

async fn get_rows(root: &Path) -> serde_json::Value {
    rows_of(state(root).await).await
}

async fn rows_of(state: AppState) -> serde_json::Value {
    rows_at(state, "/api/manage/rows").await
}

async fn rows_at(state: AppState, uri: &str) -> serde_json::Value {
    let app = router(state);
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn by_key(v: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["key"].as_str().unwrap().to_string(), r.clone()))
        .collect()
}

const CONFIG: &str = r#"
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

[[applets]]
id = "unified_index"
group = "unified_index"
command = "datalib-applet unified_index"
"#;

async fn write_root(root: &Path, config: &str, state_json: Option<&str>) {
    std::fs::create_dir_all(root.join("system")).unwrap();
    std::fs::write(root.join("config.toml"), config).unwrap();
    if let Some(j) = state_json {
        crate::record_json::write(root, j).await;
    }
}

/// The tree: a row per group, its steps and applets under it by
/// `path`, in pipeline order for the segments but config order for the
/// rows — and the names each row shows.
#[tokio::test]
async fn a_fresh_root_is_a_tree_of_never_run_rows() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(tmp.path(), CONFIG, None).await;

    let got = get_rows(tmp.path()).await;
    assert_eq!(got["ok"], true, "{got}");
    assert_eq!(got["run"], serde_json::Value::Null);
    assert_eq!(got["tree"], true);
    let types: Vec<&str> = got["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "identity",
            "actions",
            "status",
            "chips",
            "chips",
            "count",
            "timestamp",
            "timestamp",
            "timeseries"
        ]
    );
    let rows = by_key(&got);
    assert_eq!(rows.len(), 8, "{got}");
    // Nothing has counted its problems, so no row claims a green zero.
    // Nothing has counted its documents either, so no row claims a
    // zero there — an empty store and a store nobody has looked in
    // read the same to a person, and only one of them is true.
    for (key, row) in &rows {
        assert_eq!(row["problems"], serde_json::json!([]), "{key}: {row}");
        assert_eq!(row["documents"], serde_json::Value::Null, "{key}: {row}");
    }

    // `system/` is a group the config never named, with the run log
    // under it: both take disk, the log is browsable, nothing syncs.
    let system = &rows["system"];
    assert_eq!(system["kind"], "system");
    assert_eq!(system["path"], serde_json::json!(["system"]));
    assert_eq!(system["name"]["label"], "System");
    assert_eq!(system["name"]["icon"], "system");
    assert_eq!(system["type"], serde_json::Value::Null);
    assert_eq!(system["status"]["key"], "");
    let actions = system["actions"].as_array().unwrap();
    assert_eq!(actions[0]["id"], "browse");
    assert_eq!(actions[0]["enabled"], false);
    assert_eq!(actions[1]["id"], "sync");
    assert_eq!(actions[1]["enabled"], false);
    let logs = &rows["system/runs"];
    assert_eq!(logs["kind"], "system");
    assert_eq!(logs["path"], serde_json::json!(["system", "system/runs"]));
    assert_eq!(logs["name"]["label"], "Logs");
    assert_eq!(logs["actions"][0]["id"], "browse");
    assert_eq!(logs["actions"][0]["enabled"], true);
    assert_eq!(logs["actions"][1]["enabled"], false);
    assert_eq!(logs["seeds"], serde_json::json!([]));
    let keys: Vec<&str> = got["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        &keys[keys.len() - 2..],
        ["system", "system/runs"],
        "the system rows come after everything the config declares"
    );

    let slack = &rows["group:slack"];
    assert_eq!(slack["kind"], "group");
    assert_eq!(slack["name"]["label"], "Work Slack");
    assert_eq!(slack["name"]["id"], "slack");
    assert_eq!(slack["type"]["id"], "slack");
    assert_eq!(slack["type"]["label"], "Slack");
    assert_eq!(slack["type"]["icon"], "slack");
    // The Name cell leads with the type's mark; its label is the hover.
    assert_eq!(slack["name"]["icon"], "slack");
    assert_eq!(slack["name"]["detail"], "Slack");
    assert_eq!(slack["path"], serde_json::json!(["group:slack"]));
    assert_eq!(slack["status"]["key"], "never_run");
    assert_eq!(slack["status"]["from"], serde_json::Value::Null);
    assert_eq!(slack["seeds"], serde_json::json!(["slack/ingest"]));
    // Browse first, then Sync: a source with a render step has rows.
    assert_eq!(slack["actions"][0]["id"], "browse");
    assert_eq!(slack["actions"][0]["label"], "Browse this data");
    assert_eq!(slack["actions"][0]["enabled"], true);
    assert_eq!(slack["actions"][1]["id"], "sync");
    assert_eq!(slack["actions"][1]["enabled"], true);
    assert_eq!(slack["disk"]["value"], serde_json::Value::Null);
    assert_eq!(slack["disk"]["unit"], "bytes");

    let ingest = &rows["slack/ingest"];
    assert_eq!(
        ingest["path"],
        serde_json::json!(["group:slack", "slack/ingest"])
    );
    assert_eq!(ingest["phase"], "ingest");
    assert_eq!(ingest["type"]["label"], "Slack");
    assert_eq!(ingest["name"]["label"], "Ingest");
    assert_eq!(ingest["name"]["icon"], "step:ingest");
    assert_eq!(ingest["seeds"], serde_json::json!(["slack/ingest"]));

    let render = &rows["slack/render_markdown"];
    assert_eq!(render["name"]["label"], "Render markdown");
    // A step's Browse is its group's: the same button, just as enabled.
    assert_eq!(render["actions"][0], slack["actions"][0]);
    assert_eq!(ingest["actions"][0], slack["actions"][0]);
    // A render that has never run is out of date: Sync reruns it alone,
    // and says which source to sync for fresh data.
    assert_eq!(
        render["seeds"],
        serde_json::json!(["slack/render_markdown"])
    );
    assert_eq!(render["actions"][1]["enabled"], true, "{render}");
    assert!(render["actions"][1]["hint"]
        .as_str()
        .unwrap()
        .contains("sync slack/ingest"));

    // The applet shares its group's id; the group's key keeps them apart.
    let applet = &rows["unified_index"];
    assert_eq!(applet["kind"], "applet");
    assert_eq!(
        applet["path"],
        serde_json::json!(["group:unified_index", "unified_index"])
    );
    // Its health is the supervisor's word, not the runner's: up, or —
    // here, with no `datalib-applet` on the PATH — failed to start.
    // Either way it has no history, so no timestamp.
    let label = applet["status"]["label"].as_str().unwrap();
    assert!(label == "Up" || label == "Failed to start", "{applet}");
    assert_eq!(applet["status"]["at"], serde_json::Value::Null);
    assert_eq!(applet["last_synced"], serde_json::Value::Null);
    assert_eq!(applet["name"]["label"], "Unified Index (Applet)");
    assert_eq!(applet["name"]["icon"], "applet");
    assert_eq!(applet["type"]["label"], "unified_index");
    // The group reads a failed applet as its own failure, else its
    // last step, which has never run.
    let index = &rows["group:unified_index"];
    if label == "Up" {
        assert_eq!(index["status"]["key"], "never_run");
        assert_eq!(index["status_from"], "unified_index/grid_index");
    } else {
        assert_eq!(index["status"]["key"], "failed");
        assert_eq!(index["status_from"], "unified_index");
    }
    // The index group browses as the projection over every source.
    assert_eq!(index["actions"][0]["label"], "Browse every source");
    assert_eq!(index["actions"][0]["enabled"], true);
    // With no source step, the index group syncs its own steps, which
    // have never run.
    assert_eq!(index["actions"][1]["enabled"], true, "{index}");
    assert!(index["actions"][1]["hint"]
        .as_str()
        .unwrap()
        .contains("out-of-date steps"));
    assert!(applet["actions"][0]["disabled_reason"]
        .as_str()
        .unwrap()
        .contains("no rows of its own"));
    // The index group mirrors nothing, so it has no type; its mark is
    // the search it serves.
    assert_eq!(index["type"], serde_json::Value::Null);
    assert_eq!(index["name"]["icon"], "search");
}

/// A finished run: the step rows read the record, and the group reads
/// its last step — the failed render — with the child named in the
/// detail, and "last synced" from its ingest step.
#[tokio::test]
async fn a_finished_run_reaches_the_rows() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(
        tmp.path(),
        CONFIG,
        Some(
            r#"{
              "steps": {
                "slack/ingest": {
                  "succeeded": true,
                  "last_run": {
                    "run_id": "r1",
                    "started_at": "2026-08-31T10:00:00+01:00",
                    "finished_at": "2026-08-31T10:00:09+01:00",
                    "status": "succeeded",
                    "attempts": 1
                  },
                  "last_success_at": "2026-08-31T10:00:09+01:00"
                },
                "slack/render_markdown": {
                  "succeeded": false,
                  "last_success_at": "2026-08-24T10:00:11+01:00",
                  "last_run": {
                    "run_id": "r1",
                    "started_at": "2026-08-31T10:00:09+01:00",
                    "finished_at": "2026-08-31T10:00:11+01:00",
                    "status": "failed",
                    "attempts": 2,
                    "error": "bad json at line 3"
                  }
                }
              },
              "current_run": {
                "run_id": "r1",
                "started_at": "2026-08-31T10:00:00+01:00",
                "finished_at": "2026-08-31T10:00:12+01:00",
                "states": {"slack/ingest": "succeeded", "slack/render_markdown": "failed"}
              }
            }"#,
        ),
    )
    .await;

    let got = get_rows(tmp.path()).await;
    assert_eq!(got["run"]["run_id"], "r1");
    let rows = by_key(&got);

    let ingest = &rows["slack/ingest"];
    assert_eq!(ingest["status"]["key"], "succeeded");
    assert_eq!(ingest["last_synced"], "2026-08-31T10:00:09+01:00");
    assert_eq!(ingest["last_success"], "2026-08-31T10:00:09+01:00");
    assert_eq!(ingest["last_run_id"], "r1");
    assert_eq!(ingest["live_run_id"], serde_json::Value::Null);

    // The error itself is a log line, not the hover: the hover says
    // where to read it.
    let render = &rows["slack/render_markdown"];
    assert_eq!(render["status"]["key"], "failed");
    assert_eq!(render["last_synced"], "2026-08-31T10:00:11+01:00");
    assert_eq!(render["last_success"], "2026-08-24T10:00:11+01:00");
    assert_eq!(
        render["status"]["detail"],
        "double-click to open the log at the error"
    );

    let slack = &rows["group:slack"];
    assert_eq!(slack["status"]["key"], "failed");
    assert_eq!(slack["status_from"], "slack/render_markdown");
    assert_eq!(
        slack["status"]["detail"],
        "slack/render_markdown: double-click to open the log at the error"
    );
    assert_eq!(slack["last_synced"], "2026-08-31T10:00:09+01:00");
    // The status is the render's; the last success is still the
    // ingest step's, not the failed render's older one.
    assert_eq!(slack["last_success"], "2026-08-31T10:00:09+01:00");
    assert_eq!(
        slack["status"]["last_success_at"],
        "2026-08-31T10:00:09+01:00"
    );
    assert!(slack["status"].get("segments").is_none(), "{slack}");
}

/// The Problems cell reads the `problems{severity=…}` metrics a step
/// reported at the end of its last run: red and yellow on the render
/// step that counted some, a green zero on the index that counted none,
/// nothing on the ingest step that never counted — and the group shows
/// its last counting step's.
#[tokio::test]
async fn problem_counts_reach_the_rows_from_the_run_store() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(tmp.path(), CONFIG, None).await;
    {
        let w = datalib_runs::RunWriter::start(
            tmp.path(),
            "r1",
            "r1",
            None,
            datalib_runs::Retention::default(),
        )
        .unwrap();
        let metric = |step: &str, labels: &str, value: i64| datalib_runs::MetricRow {
            run_id: "r1".into(),
            step: step.into(),
            name: datalib_problems::METRIC.into(),
            labels: labels.into(),
            value,
            updated_at_utc: "2026-08-31T09:00:00+00:00".into(),
            tz_offset: None,
        };
        w.metric(metric("slack/render_markdown", "severity=error", 2));
        w.metric(metric("slack/render_markdown", "severity=warning", 5));
        w.metric(metric("unified_index/grid_index", "severity=error", 0));
        w.metric(metric("unified_index/grid_index", "severity=warning", 0));
    }

    let rows = by_key(&get_rows(tmp.path()).await);
    let chips = |key: &str| -> Vec<(String, String)> {
        rows[key]["problems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["kind"].as_str().unwrap().to_string(),
                    c["text"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let red_and_yellow = vec![
        ("error".to_string(), "2 errors".to_string()),
        ("warning".to_string(), "5 warnings".to_string()),
    ];
    assert_eq!(chips("slack/render_markdown"), red_and_yellow);
    assert_eq!(
        chips("group:slack"),
        red_and_yellow,
        "the group shows render's"
    );
    assert_eq!(
        chips("slack/ingest"),
        vec![],
        "never counted: blank, not zero"
    );
    assert_eq!(
        chips("unified_index/grid_index"),
        vec![("ok".to_string(), "0".to_string())]
    );
    assert_eq!(
        chips("group:unified_index"),
        vec![("ok".to_string(), "0".to_string())]
    );
}

/// An entry the loader drops still has a row — it is still in the
/// file — and its status says so, outranking whatever the record
/// remembers. The group it is under is not dropped with it.
/// A source that renders nothing has no rows at all, so its Browse is
/// disabled and says so, rather than opening an empty grid onto a
/// source that looks broken.
#[tokio::test]
async fn a_download_only_source_cannot_be_browsed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = format!(
        "{CONFIG}
[[groups]]
id = \"photos\"
type = \"media\"

[[steps]]
group = \"photos\"
function = \"ingest\"
"
    );
    write_root(tmp.path(), &config, None).await;

    let got = get_rows(tmp.path()).await;
    assert_eq!(got["ok"], true, "{got}");
    let rows = by_key(&got);
    let photos = &rows["group:photos"];
    assert_eq!(photos["actions"][0]["id"], "browse");
    assert_eq!(photos["actions"][0]["enabled"], false);
    assert!(photos["actions"][0]["disabled_reason"]
        .as_str()
        .unwrap()
        .contains("no render step"));
    // Its step says the same, not something of its own.
    assert_eq!(rows["photos/ingest"]["actions"][0], photos["actions"][0]);
}

/// A download step's row names its raw store once the file exists —
/// what Browse opens in the desktop app — and no other row names one.
#[tokio::test]
async fn a_download_step_names_its_raw_store_once_it_exists() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(tmp.path(), CONFIG, None).await;
    let ingest = tmp.path().join("slack/ingest");
    std::fs::create_dir_all(&ingest).unwrap();
    std::fs::create_dir_all(tmp.path().join("slack/render_markdown")).unwrap();
    std::fs::write(
        tmp.path()
            .join("slack/render_markdown/indexed_markdown.doltlite_db"),
        b"CTLD",
    )
    .unwrap();
    let uri = "/api/manage/rows?refresh=1";

    let before = by_key(&rows_at(state(tmp.path()).await, uri).await);
    assert_eq!(
        before["slack/ingest"]["raw_store_path"],
        serde_json::Value::Null
    );

    std::fs::write(ingest.join("entities.doltlite_db"), b"CTLD").unwrap();
    let rows = by_key(&rows_at(state(tmp.path()).await, uri).await);
    let named: Vec<(&String, &serde_json::Value)> = rows
        .iter()
        .filter(|(_, r)| !r["raw_store_path"].is_null())
        .collect();
    assert_eq!(named.len(), 1, "{named:?}");
    assert_eq!(named[0].0, "slack/ingest");
    assert_eq!(
        Path::new(named[0].1["raw_store_path"].as_str().unwrap()),
        ingest.join("entities.doltlite_db")
    );
}

#[tokio::test]
async fn a_dropped_entry_keeps_its_row_and_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CONFIG.replace(
        "function = \"render_markdown\"\n",
        "function = \"render_markdown\"\ntitle = \"not a key\"\n",
    );
    write_root(tmp.path(), &config, None).await;

    let got = get_rows(tmp.path()).await;
    assert_eq!(got["ok"], true, "{got}");
    let rows = by_key(&got);
    let render = &rows["slack/render_markdown"];
    assert_eq!(render["status"]["key"], "config_rejected");
    assert_eq!(render["status"]["label"], "Not loaded");
    assert!(
        render["dropped"]["message"]
            .as_str()
            .unwrap()
            .contains("title"),
        "{render}"
    );
    assert!(render["actions"][1]["disabled_reason"]
        .as_str()
        .unwrap()
        .starts_with("Not in the pipeline"));
    // The fan-in that read it is blocked by it, and says so.
    let index = &rows["unified_index/grid_index"];
    assert_eq!(index["status"]["key"], "config_blocked");
    // The group itself still loads and still runs from its ingest step.
    let slack = &rows["group:slack"];
    assert_eq!(slack["dropped"], serde_json::Value::Null);
    assert_eq!(slack["seeds"], serde_json::json!(["slack/ingest"]));
}

/// A file that is not TOML has no rows to show and says so, rather
/// than 500ing or returning an empty table that reads as "no sources".
#[tokio::test]
async fn a_file_that_is_not_toml_is_an_error_not_an_empty_table() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(tmp.path(), "[[steps", None).await;
    let got = get_rows(tmp.path()).await;
    assert_eq!(got["ok"], false);
    assert!(got["error"].as_str().is_some_and(|e| !e.is_empty()));
    assert_eq!(got["rows"].as_array().unwrap().len(), 0);
}

/// The Documents cell reads the `documents` metric the render step
/// reports, whole store: the source's own row and the group above it
/// show it, and every step that never reported it stays blank. That a
/// counted zero survives as a zero is `manage::documents`' own test.
#[tokio::test]
async fn document_counts_reach_the_rows_from_the_run_store() {
    let tmp = tempfile::tempdir().unwrap();
    write_root(tmp.path(), CONFIG, None).await;
    {
        let w = datalib_runs::RunWriter::start(
            tmp.path(),
            "r1",
            "r1",
            None,
            datalib_runs::Retention::default(),
        )
        .unwrap();
        let metric = |step: &str, value: i64| datalib_runs::MetricRow {
            run_id: "r1".into(),
            step: step.into(),
            name: datalib_metrics::DOCUMENTS.into(),
            labels: String::new(),
            value,
            updated_at_utc: "2026-08-31T09:00:00+00:00".into(),
            tz_offset: None,
        };
        w.metric(metric("slack/render_markdown", 1204));
    }

    let rows = by_key(&get_rows(tmp.path()).await);
    assert_eq!(rows["slack/render_markdown"]["documents"], 1204);
    assert_eq!(
        rows["group:slack"]["documents"], 1204,
        "the group shows its render step's"
    );
    for key in ["slack/ingest", "unified_index/grid_index", "system"] {
        assert_eq!(
            rows[key]["documents"],
            serde_json::Value::Null,
            "{key}: never counted is blank, not zero"
        );
    }
}
