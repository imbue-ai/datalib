//! Notion synth → playback → download round-trip.

use std::collections::HashMap;
use std::time::Duration;

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_notion::download::{fetch, FetchOptions, RawDb};
use datalib_etl_notion::synthesize::NotionSynth;
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
    let uid = "00000001-1701-4d00-8000-000000000001";
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
    k.insert("id".into(), Value::String(pid.into()));
    write_event(
        &api,
        "notion_page_markdown",
        k,
        json!({
            "object": "page_markdown",
            "id": pid,
            "markdown": "# Hello\n\nA paragraph.\n",
            "truncated": false,
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
            "discussion_id": "dddddddd-1111-2222-3333-444444444444",
            "rich_text": [],
            "created_by": {"object": "user", "id": uid},
            "parent": {"type": "block_id", "block_id": bid},
        }),
    );

    // A user the page's `created_by` points at. Notion resolves comment
    // authors inline but not page authors, so this is the only way a
    // page gets a name instead of an id prefix.
    let mut k = Map::new();
    k.insert("id".into(), Value::String(uid.into()));
    write_event(
        &api,
        "notion_user",
        k,
        json!({"object": "user", "id": uid, "name": "Jean-Luc Picard", "type": "person"}),
    );

    // The block the comment hangs off. A comment carries no quote of
    // what it is about, so this is fetched per *commented* block.
    let mut k = Map::new();
    k.insert("id".into(), Value::String(bid.into()));
    write_event(
        &api,
        "notion_anchor_block",
        k,
        json!({
            "object": "block", "id": bid, "type": "paragraph",
            "paragraph": {"rich_text": [{"plain_text": "Warp core alignment"}]},
        }),
    );

    let report = NotionSynth::new(&api).synthesize(&playback).unwrap();
    // 1 page + 1 markdown + 1 comments + 1 user + 1 anchor block = 5
    assert_eq!(report.fixtures_written, 5);

    std::env::set_var(PLAYBACK_ENV, &playback);

    // The test owns the store: one connection for the download and the
    // assertions both, because two is what breaks a doltlite file.
    let out = RawDb::open(&out_db).await.unwrap();
    let summary = fetch(FetchOptions {
        subtree_pages: vec![pid.to_string()],
        sleep_between: Duration::ZERO,
        ..FetchOptions::new(out.clone())
    })
    .await
    .unwrap();
    assert_eq!(summary.new_pages, 1);

    let pages = out.load_pages().await.unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["id"], pid);
    let bodies = out.load_page_markdown().await.unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].0, pid);
    assert!(bodies[0].1.contains("A paragraph."));
    let comments = out.load_comments().await.unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].0["id"], cid);

    // Page authors need the users table; comment authors do not.
    let names = out.load_user_names().await.unwrap();
    assert_eq!(names.get(uid).map(String::as_str), Some("Jean-Luc Picard"));

    // And the comment's anchor must be recoverable, or the thread reads
    // as a reply to nothing.
    let anchors = out.load_comment_anchors().await.unwrap();
    assert_eq!(
        anchors.get(bid).map(String::as_str),
        Some("Warp core alignment")
    );
}
