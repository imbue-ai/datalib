//! Notion synth → playback → extract round-trip.
//!
//! Seeds a JSONL fixture tree (the on-disk shape NotionSynth reads),
//! synthesizes playback fixtures, then drives `extract::fetch` against
//! a fresh doltlite db. Asserts the round-trip lands one page / block
//! / comment per input record.

use std::collections::HashMap;
use std::time::Duration;

use frankweiler_etl::event_store::{diff_and_save, make_record};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_notion::extract::{fetch, FetchOptions, RawDb};
use frankweiler_etl_notion::synthesize::NotionSynth;
use serde_json::{json, Map, Value};
use tempfile::tempdir;

fn write_event(api: &std::path::Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notion_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("jsonl_input");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out.doltlite_db");
    std::fs::create_dir_all(&api).unwrap();

    let pid = "11111111-2222-3333-4444-555555555555";
    let bid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let cid = "cccccccc-1111-2222-3333-444444444444";

    let mut k = Map::new();
    k.insert("id".into(), Value::String(pid.into()));
    write_event(
        &api,
        "notion_official_page",
        k,
        json!({
            "id": pid,
            "object": "page",
            "last_edited_time": "2025-01-01T00:00:00.000Z",
            "parent": {"type": "workspace"},
        }),
    );
    let mut k = Map::new();
    k.insert("id".into(), Value::String(bid.into()));
    k.insert("page_id".into(), Value::String(pid.into()));
    write_event(
        &api,
        "notion_official_block",
        k,
        json!({
            "id": bid,
            "type": "paragraph",
            "has_children": false,
            "parent": {"type": "page_id", "page_id": pid},
        }),
    );
    let mut k = Map::new();
    k.insert("id".into(), Value::String(cid.into()));
    k.insert("page_id".into(), Value::String(pid.into()));
    write_event(
        &api,
        "notion_official_comment",
        k,
        json!({
            "id": cid,
            "object": "comment",
            "rich_text": [],
            "parent": {"type": "page_id", "page_id": pid},
        }),
    );

    let report = NotionSynth::new(&api).synthesize(&playback).unwrap();
    // 1 page + 1 children (page-level; block has no children) + 1 comments = 3
    assert_eq!(report.fixtures_written, 3);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        db_path: out_db.clone(),
        subtree_pages: vec![pid.to_string()],
        sleep_between: Duration::ZERO,
        ..FetchOptions::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.new_pages, 1);

    let out = RawDb::open(&out_db).await.unwrap();
    let pages = out.load_pages().await.unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["id"], pid);
    let blocks = out.load_blocks().await.unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].0["id"], bid);
    let comments = out.load_comments().await.unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].0["id"], cid);
}
