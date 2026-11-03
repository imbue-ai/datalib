//! An ingest that finds nothing new reports the version it reported last
//! time, so no step that reads it runs again. Every Slack run commits its
//! own record (`sync_runs`) and stamps, which moved the store's head, and
//! the head was the version: a custom step reading a Slack source woke on
//! every refresh and found nothing to do. The real binary, twice, against
//! one playback.
use std::path::Path;
use std::process::{Command, Stdio};

use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use datalib_etl_slack::synthesize::SlackSynth;
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::{json, Value};

const STEP: &str = "work-slack/ingest";

/// The run's outcome line, and every checkpoint version it announced.
fn ingest(root: &Path, playback: &Path, params: &Path, now: &str) -> (String, Vec<String>) {
    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("--params-file")
        .arg(params)
        .env("DATALIB_DAG_STEP", STEP)
        .env("DATALIB_DAG_GROUP", "work-slack")
        .env("DATALIB_DAG_GROUP_TYPE", "slack")
        .env("DATALIB_DAG_FUNCTION", "ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_DAG_NOW", now)
        .env("DATALIB_HTTP_PLAYBACK", playback)
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let outcome = events
        .iter()
        .rev()
        .find(|v| v["event"] == "outcome")
        .expect("an outcome line");
    let version = outcome["outputs"][0]["version"]
        .as_str()
        .unwrap_or_else(|| panic!("a version: {outcome}"))
        .to_string();
    let checkpoints = events
        .iter()
        .filter(|v| v["event"] == "checkpoint")
        .filter_map(|v| v["version"].as_str().map(str::to_string))
        .collect();
    (version, checkpoints)
}

#[tokio::test]
async fn an_ingest_that_finds_nothing_new_keeps_its_version() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("root");
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    std::fs::create_dir_all(&root).unwrap();
    // Two channels with one message each, and `client.counts` showing
    // both with nothing newer: a second sync has nothing to fetch.
    const T1: &str = "1735689600.000100";
    record_auth(&api).unwrap();
    record_users(&api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(
        &api,
        CHANNEL_TYPES,
        json!([
            {"id": "C1", "name": "bridge", "is_member": true},
            {"id": "C2", "name": "engineering", "is_member": true},
        ]),
    )
    .unwrap();
    for c in ["C1", "C2"] {
        History::cold(c)
            .record(
                &api,
                json!([{"ts": T1, "user": "U1", "text": format!("in {c}")}]),
            )
            .unwrap();
    }
    record_call(
        &api,
        "client.counts",
        json!({}),
        json!({"ok": true, "mpims": [], "ims": [], "channels": [
            {"id": "C1", "latest": T1, "last_read": T1},
            {"id": "C2", "latest": T1, "last_read": T1},
        ]}),
    )
    .unwrap();
    // The search for replies to threads history will not read again,
    // which the second sync makes, finding none.
    record_call(
        &api,
        "search.messages",
        json!({"query": "is:thread in:<#C1> in:<#C2> after:2026-11-01 before:2026-11-05",
               "count": "100", "sort": "timestamp", "sort_dir": "desc", "page": "1"}),
        json!({"ok": true, "messages": {
            "total": 0,
            "paging": {"count": 100, "total": 0, "page": 1, "pages": 1},
            "matches": [],
        }}),
    )
    .unwrap();
    SlackSynth::new(&api).synthesize(&playback).unwrap();
    let params = d.path().join("params.json");
    std::fs::write(
        &params,
        json!({"api": {"media": false, "dms": false, "refresh_window_days": 0}}).to_string(),
    )
    .unwrap();
    let store = datalib_etl::raw_layout::entities_db(&root.join(STEP));

    let (first, _) = ingest(&root, &playback, &params, "2026-11-03T08:00:00+00:00");
    let head_after_first = datalib_etl::doltlite_raw::head_commit_at_path(&store)
        .await
        .unwrap();
    let (second, announced) = ingest(&root, &playback, &params, "2026-11-03T09:00:00+00:00");
    let head_after_second = datalib_etl::doltlite_raw::head_commit_at_path(&store)
        .await
        .unwrap();

    assert_ne!(
        head_after_first, head_after_second,
        "the second run committed its own record, as before"
    );
    assert_eq!(first, second, "and reported the version it had");
    assert!(
        announced.iter().all(|v| *v == first),
        "no checkpoint announced a new version: {announced:?}"
    );
}
