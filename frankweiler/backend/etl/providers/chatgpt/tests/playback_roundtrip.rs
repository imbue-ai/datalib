//! End-to-end synth → playback → extract round-trip.
//!
//! Builds a fake on-disk ChatGPT snapshot, runs the HTTP fixture
//! synthesizer over it, points `FRANKWEILER_HTTP_PLAYBACK` at the
//! resulting fixture tree, then drives `extract::fetch` against a fresh
//! output directory. Asserts the rehydrated snapshot matches the input
//! (modulo the per-conv synthetic keys extract re-stamps each run).
//!
//! Lives in its own integration-test file so the process-wide
//! `FRANKWEILER_HTTP_PLAYBACK` env var can't race other tests.

use std::fs;
use std::time::Duration;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_chatgpt::extract::{fetch, FetchOptions};
use frankweiler_etl_chatgpt::synthesize::ChatgptSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_json(path: &std::path::Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn chatgpt_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let out = d.path().join("out_snapshot");

    // Seed: me + 2 conversations in the listing, one of them has a
    // per-conv file on disk.
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

    // Process-wide; safe because this binary holds exactly one test.
    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        out_dir: out.clone(),
        max_pages: None,
        limit: None,
        sleep_between: Duration::ZERO,
        conv_uuid: None,
    })
    .await
    .unwrap();
    assert_eq!(summary.fetched, 2);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.listing, 2);

    // me.json round-trips byte-for-byte content (modulo pretty-printing).
    let me_in: Value = serde_json::from_slice(&fs::read(api.join("me.json")).unwrap()).unwrap();
    let me_out: Value = serde_json::from_slice(&fs::read(out.join("me.json")).unwrap()).unwrap();
    assert_eq!(me_in, me_out);

    // conversations.json: extract writes the listing it walked, which
    // should equal the synthesized page items (= our input listing).
    let listing_out: Value =
        serde_json::from_slice(&fs::read(out.join("conversations.json")).unwrap()).unwrap();
    assert_eq!(listing_out, listing);

    // Each per-conv file should match the input, ignoring extract's
    // freshly-stamped synthetic keys.
    for id in ["c-a", "c-b"] {
        let mut got: Value = serde_json::from_slice(
            &fs::read(out.join(format!("conversations/{id}.json"))).unwrap(),
        )
        .unwrap();
        let obj = got.as_object_mut().unwrap();
        assert!(
            obj.remove("_fetched_at").is_some(),
            "{id}: missing _fetched_at"
        );
        assert!(
            obj.remove("_listing_update_time").is_some(),
            "{id}: missing _listing_update_time"
        );
        let want: Value = serde_json::from_slice(
            &fs::read(api.join(format!("conversations/{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(got, want, "{id} body mismatch");
    }
}
