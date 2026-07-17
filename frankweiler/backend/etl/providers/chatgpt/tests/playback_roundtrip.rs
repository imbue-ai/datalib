//! End-to-end synth → playback → download round-trip.
//!
//! Builds a fake on-disk ChatGPT JSON snapshot (the format the
//! synthesizer reads), runs the HTTP fixture synthesizer over it,
//! points `FRANKWEILER_HTTP_PLAYBACK` at the resulting fixture tree,
//! then drives `download::fetch` against a fresh doltlite database.
//! Asserts the rehydrated DB matches the input.
//!
//! Lives in its own integration-test file so the process-wide
//! `FRANKWEILER_HTTP_PLAYBACK` env var can't race other tests.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_chatgpt::download::{
    db::block_on_load_all, db::db_path_for, fetch, FetchOptions,
};
use frankweiler_etl_chatgpt::synthesize::ChatgptSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_json(path: &std::path::Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out_snapshot.doltlite_db");

    write_json(
        &api.join("me.json"),
        &json!({"id": "u-1", "email": "x@y.test"}),
    );
    let listing = json!([
        {"id": "c-a", "update_time": 1.0, "title": "A"},
        {"id": "c-b", "update_time": 2.0, "title": "B"},
    ]);
    write_json(&api.join("conversations.json"), &listing);
    write_json(
        &api.join("conversations/c-a.json"),
        &json!({"id": "c-a", "mapping": {"n1": {"id": "n1"}}, "title": "A"}),
    );
    write_json(
        &api.join("conversations/c-b.json"),
        &json!({"id": "c-b", "mapping": {}, "title": "B"}),
    );

    let report = ChatgptSynth::new(&api).synthesize(&playback).unwrap();
    // me + 1 listing page + 1 terminator + 2 conv = 5
    assert_eq!(report.fixtures_written, 5);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        db_path: out_db.clone(),
        max_pages: None,
        limit: None,
        sleep_between: Duration::ZERO,
        conv_uuids: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.fetched, 2);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.listing, 2);

    // Verify what landed in the DB matches the input snapshot.
    let raw = block_on_load_all(&db_path_for(&out_db)).expect("load db");
    let me = raw.me.expect("me row present");
    assert_eq!(me["id"], "u-1");
    assert_eq!(me["email"], "x@y.test");

    let by_id: HashMap<String, Value> = raw
        .conversations
        .into_iter()
        .map(|c| (c.id, c.payload))
        .collect();
    for id in ["c-a", "c-b"] {
        let want: Value = serde_json::from_slice(
            &fs::read(api.join(format!("conversations/{id}.json"))).unwrap(),
        )
        .unwrap();
        // Payload is the raw upstream response — no `_fetched_at` or
        // `_listing_update_time` polluting the body anymore.
        let got = by_id.get(id).expect("conv in db");
        assert_eq!(got, &want, "{id} body mismatch");
    }
}
