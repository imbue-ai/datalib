//! Anthropic synth → playback → extract round-trip.
//!
//! Builds a JSON snapshot, synthesizes playback fixtures, runs
//! `extract::fetch` against a fresh doltlite db, and asserts the
//! rehydrated conversations match the input. With the doltlite port
//! we store the **raw** API payload in `conversations.payload`, so
//! comparisons happen against the raw response shape rather than the
//! normalized export shape.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_anthropic::extract::{
    db::block_on_load_all, db::db_path_for, fetch, FetchOptions,
};
use frankweiler_etl_anthropic::synthesize::AnthropicSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out_snapshot.doltlite_db");
    fs::create_dir_all(&api).unwrap();

    // Pre-normalized conversation: account set, _source set, no chat_messages
    // so normalize is a no-op. The synth serves these back directly, so
    // when extract refetches them via playback, we get the same body.
    let convs = json!([
        {
            "uuid": "c1",
            "name": "First",
            "updated_at": "2025-01-02T00:00:00Z",
            "organization_uuid": "org-a",
            "account": {"uuid": "acct-1"},
            "chat_messages": [],
            "_source": {"via": "claude.ai/api", "org_uuid": "org-a"},
        },
        {
            "uuid": "c2",
            "name": "Second",
            "updated_at": "2025-01-01T00:00:00Z",
            "organization_uuid": "org-b",
            "account": {"uuid": "acct-1"},
            "chat_messages": [],
            "_source": {"via": "claude.ai/api", "org_uuid": "org-b"},
        },
    ]);
    fs::write(
        api.join("conversations.json"),
        serde_json::to_vec_pretty(&convs).unwrap(),
    )
    .unwrap();
    fs::write(
        api.join("users.json"),
        serde_json::to_vec_pretty(&json!([{"uuid": "acct-1"}])).unwrap(),
    )
    .unwrap();

    let report = AnthropicSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(report.fixtures_written, 5);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        db_path: out_db.clone(),
        // Point export_dir at our input snapshot so users.json gets
        // ingested before the listing pass needs account_uuid.
        export_dir: Some(api.clone()),
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.fetched, 2);
    assert_eq!(summary.total, 2);

    let raw = block_on_load_all(&db_path_for(&out_db)).expect("load db");
    let by_id: HashMap<String, Value> = raw
        .conversations
        .into_iter()
        .map(|c| (c.id, c.payload))
        .collect();
    let want_arr: Vec<Value> = convs.as_array().cloned().unwrap();
    let by_uuid_want: HashMap<String, &Value> = want_arr
        .iter()
        .map(|c| (c["uuid"].as_str().unwrap().to_string(), c))
        .collect();
    assert_eq!(by_id.len(), by_uuid_want.len());
    for (uuid, got) in &by_id {
        let w = by_uuid_want.get(uuid).expect("uuid missing from input");
        assert_eq!(got, *w, "{uuid} mismatch");
    }
}
