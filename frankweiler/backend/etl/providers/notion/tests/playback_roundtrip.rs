//! Notion synth → playback → extract round-trip.

use std::time::Duration;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_notion::extract::{fetch, FetchOptions, RawDb};
use frankweiler_etl_notion::synthesize::NotionSynth;
use serde_json::json;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notion_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let input_db = d.path().join("input.doltlite_db");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out.doltlite_db");

    // Dashed 32-hex UUIDs so format_uuid() in extract matches.
    let pid = "11111111-2222-3333-4444-555555555555";
    let bid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let cid = "cccccccc-1111-2222-3333-444444444444";

    let db = RawDb::open(&input_db).await.unwrap();
    db.upsert_pages(&[(
        pid.into(),
        None,
        Some("2025-01-01T00:00:00.000Z".into()),
        Some(
            serde_json::to_string(&json!({
                "id": pid,
                "object": "page",
                "last_edited_time": "2025-01-01T00:00:00.000Z",
                "parent": {"type": "workspace"},
            }))
            .unwrap(),
        ),
    )])
    .await
    .unwrap();
    db.upsert_blocks(&[(
        bid.into(),
        Some(pid.into()),
        Some(pid.into()),
        None,
        Some(
            serde_json::to_string(&json!({
                "id": bid,
                "type": "paragraph",
                "has_children": false,
                "parent": {"type": "page_id", "page_id": pid},
            }))
            .unwrap(),
        ),
    )])
    .await
    .unwrap();
    db.upsert_comments(&[(
        cid.into(),
        pid.into(),
        Some(pid.into()),
        serde_json::to_string(&json!({"id": cid, "object": "comment", "rich_text": []})).unwrap(),
    )])
    .await
    .unwrap();
    drop(db);

    let report = NotionSynth::new(&input_db).synthesize(&playback).unwrap();
    // 1 page + 1 children + 1 comments = 3 (block has no children, no extra)
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

    // Round-trip: load the out db and confirm the same payloads landed.
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
