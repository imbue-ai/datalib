//! GitLab synth → playback → extract round-trip.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::event_store::{diff_and_save, make_record};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_gitlab::extract::{
    block_on_load_all, db_path_for, fetch, FetchOptions, ENTITY_DISCUSSION, ENTITY_MR, ENTITY_SELF,
};
use frankweiler_etl_gitlab::synthesize::GitlabSynth;
use serde_json::{json, Map, Value};
use tempfile::tempdir;

fn write_event(api: &std::path::Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gitlab_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_events");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out.doltlite_db");
    fs::create_dir_all(&api).unwrap();

    let mut k = Map::new();
    k.insert("user_id".into(), json!(7));
    write_event(
        &api,
        ENTITY_SELF,
        k,
        json!({"id": 7, "username": "tt", "web_url": "https://gitlab.com/tt"}),
    );

    let proj = "ns/proj";
    let iid: u64 = 12;
    let mr_raw = json!({
        "iid": iid,
        "web_url": format!("https://gitlab.com/{proj}/-/merge_requests/{iid}"),
        "state": "opened",
        "source_branch": "feat",
        "target_branch": "main",
    });
    let mut k = Map::new();
    k.insert("project_full_path".into(), json!(proj));
    k.insert("mr_iid".into(), json!(iid));
    write_event(&api, ENTITY_MR, k, mr_raw.clone());

    let disc_raw = json!({"id": "abc", "individual_note": false, "notes": [{"updated_at": "2025-01-01T00:00:00Z"}]});
    let mut k = Map::new();
    k.insert("project_full_path".into(), json!(proj));
    k.insert("mr_iid".into(), json!(iid));
    k.insert("discussion_id".into(), json!("abc"));
    write_event(&api, ENTITY_DISCUSSION, k, disc_raw.clone());

    let report = GitlabSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(report.fixtures_written, 6);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        db_path: out_db.clone(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..FetchOptions::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.new_mrs, 1);
    assert_eq!(summary.new_discussions, 1);

    let raw = block_on_load_all(&db_path_for(&out_db)).expect("load db");
    let me = raw.self_identity.expect("self identity present");
    assert_eq!(me["id"], 7);
    assert_eq!(me["username"], "tt");

    assert_eq!(raw.merge_requests.len(), 1);
    assert_eq!(raw.merge_requests[0].payload, mr_raw);
    assert_eq!(raw.discussions.len(), 1);
    assert_eq!(raw.discussions[0].payload, disc_raw);
}
