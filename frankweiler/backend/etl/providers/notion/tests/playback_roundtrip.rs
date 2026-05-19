//! Notion synth → playback → extract round-trip.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::event_store::{diff_and_save, load_latest_by_key, make_record};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_notion::extract::{
    fetch, FetchOptions, ENTITY_BLOCK, ENTITY_COMMENT, ENTITY_PAGE,
};
use frankweiler_etl_notion::synthesize::NotionSynth;
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
async fn notion_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_events");
    let playback = d.path().join("playback");
    let out = d.path().join("out_events");
    fs::create_dir_all(&api).unwrap();

    // Dashed 32-hex UUIDs so format_uuid() in extract matches.
    let pid = "11111111-2222-3333-4444-555555555555";

    let mut k = Map::new();
    k.insert("id".into(), json!(pid));
    write_event(
        &api,
        ENTITY_PAGE,
        k,
        json!({
            "id": pid,
            "object": "page",
            "last_edited_time": "2025-01-01T00:00:00.000Z",
            "parent": {"type": "workspace"},
        }),
    );

    let bid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let mut k = Map::new();
    k.insert("id".into(), json!(bid));
    k.insert("page_id".into(), json!(pid));
    write_event(
        &api,
        ENTITY_BLOCK,
        k,
        json!({
            "id": bid,
            "type": "paragraph",
            "has_children": false,
            "parent": {"type": "page_id", "page_id": pid},
        }),
    );

    let cid = "cccccccc-1111-2222-3333-444444444444";
    let mut k = Map::new();
    k.insert("id".into(), json!(cid));
    k.insert("page_id".into(), json!(pid));
    write_event(
        &api,
        ENTITY_COMMENT,
        k,
        json!({"id": cid, "object": "comment", "rich_text": []}),
    );

    let report = NotionSynth::new(&api).synthesize(&playback).unwrap();
    // 1 page + 1 children + 1 comments = 3 (block has no children, no extra)
    assert_eq!(report.fixtures_written, 3);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        out_dir: out.clone(),
        subtree_pages: vec![pid.to_string()],
        sleep_between: Duration::ZERO,
        ..FetchOptions::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.new_pages, 1);
    assert_eq!(summary.new_blocks, 1);
    assert_eq!(summary.new_comments, 1);

    let key_id = |r: &Value| r["id"].as_str().unwrap_or("").to_string();
    for entity in [ENTITY_PAGE, ENTITY_BLOCK, ENTITY_COMMENT] {
        let want = raws_by_key(&api, entity, key_id);
        let got = raws_by_key(&out, entity, key_id);
        assert_eq!(got, want, "{entity} mismatch");
    }
}
