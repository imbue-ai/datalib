//! GitLab synth → playback → extract round-trip.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::event_store::{diff_and_save, load_latest_by_key, make_record};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_gitlab::extract::{
    fetch, FetchOptions, ENTITY_DISCUSSION, ENTITY_MR, ENTITY_SELF,
};
use frankweiler_etl_gitlab::synthesize::GitlabSynth;
use serde_json::{json, Map, Value};
use tempfile::tempdir;

fn write_event(api: &std::path::Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

fn raws_by_key<F: FnMut(&Value) -> String>(
    dir: &std::path::Path,
    entity: &str,
    key_of: F,
) -> HashMap<String, Value> {
    load_latest_by_key(dir, entity, key_of)
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k, v.get("raw").cloned().unwrap_or(Value::Null)))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn gitlab_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_events");
    let playback = d.path().join("playback");
    let out = d.path().join("out_events");
    fs::create_dir_all(&api).unwrap();

    // self
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
    let mut k = Map::new();
    k.insert("project_full_path".into(), json!(proj));
    k.insert("mr_iid".into(), json!(iid));
    write_event(
        &api,
        ENTITY_MR,
        k,
        json!({
            "iid": iid,
            "web_url": format!("https://gitlab.com/{proj}/-/merge_requests/{iid}"),
            "state": "opened",
            "source_branch": "feat",
            "target_branch": "main",
        }),
    );

    let mut k = Map::new();
    k.insert("project_full_path".into(), json!(proj));
    k.insert("mr_iid".into(), json!(iid));
    k.insert("discussion_id".into(), json!("abc"));
    write_event(
        &api,
        ENTITY_DISCUSSION,
        k,
        json!({"id": "abc", "individual_note": false, "notes": [{"updated_at": "2025-01-01T00:00:00Z"}]}),
    );

    let report = GitlabSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(report.fixtures_written, 6);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        out_dir: out.clone(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..FetchOptions::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.new_mrs, 1);
    assert_eq!(summary.new_discussions, 1);

    let key_mr = |r: &Value| {
        format!(
            "{}!{}",
            r["project_full_path"].as_str().unwrap_or(""),
            r["mr_iid"]
        )
    };
    let key_disc = |r: &Value| {
        format!(
            "{}!{}#{}",
            r["project_full_path"].as_str().unwrap_or(""),
            r["mr_iid"],
            r["discussion_id"].as_str().unwrap_or("")
        )
    };

    let want_mr = raws_by_key(&api, ENTITY_MR, key_mr);
    let got_mr = raws_by_key(&out, ENTITY_MR, key_mr);
    assert_eq!(got_mr, want_mr);

    let want_d = raws_by_key(&api, ENTITY_DISCUSSION, key_disc);
    let got_d = raws_by_key(&out, ENTITY_DISCUSSION, key_disc);
    assert_eq!(got_d, want_d);
}
