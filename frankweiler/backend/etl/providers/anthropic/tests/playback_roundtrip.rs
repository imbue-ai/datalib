//! Anthropic synth → playback → extract round-trip.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_anthropic::extract::{fetch, FetchOptions};
use frankweiler_etl_anthropic::synthesize::AnthropicSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

#[tokio::test(flavor = "current_thread")]
async fn anthropic_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let out = d.path().join("out_snapshot");
    fs::create_dir_all(&api).unwrap();

    // Pre-normalized conversation: account set, _source set, no chat_messages
    // so normalize is a no-op.
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
    // /organizations + 2 listings + 2 details = 5
    assert_eq!(report.fixtures_written, 5);

    std::env::set_var(PLAYBACK_ENV, &playback);

    // Pre-seed users.json in out_dir so account_uuid is found without
    // depending on export_dir.
    fs::create_dir_all(&out).unwrap();
    fs::write(
        out.join("users.json"),
        serde_json::to_vec_pretty(&json!([{"uuid": "acct-1"}])).unwrap(),
    )
    .unwrap();

    let summary = fetch(FetchOptions {
        out_dir: out.clone(),
        export_dir: None,
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.fetched, 2);
    assert_eq!(summary.total, 2);

    let want: Vec<Value> = convs.as_array().cloned().unwrap();
    let got: Value =
        serde_json::from_slice(&fs::read(out.join("conversations.json")).unwrap()).unwrap();
    let got_arr: Vec<Value> = got.as_array().cloned().unwrap();
    assert_eq!(got_arr.len(), want.len());

    // Compare by uuid (extract sorts by updated_at desc; we don't rely
    // on that ordering — just on per-conv equality).
    let by_uuid_want: HashMap<String, &Value> = want
        .iter()
        .map(|c| (c["uuid"].as_str().unwrap().to_string(), c))
        .collect();
    for g in &got_arr {
        let uuid = g["uuid"].as_str().unwrap();
        let w = by_uuid_want.get(uuid).expect("uuid missing from input");
        assert_eq!(&g, w, "{uuid} mismatch");
    }
}
