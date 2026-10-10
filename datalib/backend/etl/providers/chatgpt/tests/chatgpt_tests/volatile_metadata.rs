//! ChatGPT's own product metadata on a message is not a change to the
//! conversation.

use std::fs;
use std::time::Duration;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_chatgpt::ingest::{db::block_on_load_all, db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_chatgpt::synthesize::ChatgptSynth;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::{json, Value};
use tempfile::tempdir;

const CONV: &str = "c-enterprise";

fn write_json(path: &std::path::Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

/// One conversation as the detail endpoint answers it, its assistant
/// message carrying the models the switcher refuses, as of `denied`.
fn detail(denied: &[&str]) -> Value {
    let deny: Vec<Value> = denied
        .iter()
        .map(|slug| json!({"slug": slug, "context": "regenerate", "reason": "unsupported"}))
        .collect();
    json!({
        "id": CONV,
        "title": "Warp core diagnostics",
        "update_time": 1_710_959_331.420159_f64,
        "mapping": {
            "root": {"id": "root", "message": null, "parent": null, "children": ["q1"]},
            "q1": {"id": "q1", "parent": "root", "children": ["a1"], "message": {
                "id": "q1", "author": {"role": "user"},
                "content": {"content_type": "text", "parts": ["Is the warp core stable, Data?"]},
                "metadata": {}
            }},
            "a1": {"id": "a1", "parent": "q1", "children": [], "message": {
                "id": "a1", "author": {"role": "assistant"},
                "content": {"content_type": "text", "parts": ["Within tolerance, Geordi."]},
                "metadata": {"model_slug": "soong-4", "model_switcher_deny": deny}
            }}
        }
    })
}

/// Fetches the named conversation once against `api` as it stands now,
/// and returns the content row it left.
async fn sync(
    api: &std::path::Path,
    playback: &std::path::Path,
    out_db: &std::path::Path,
) -> Value {
    let _ = fs::remove_dir_all(playback);
    ChatgptSynth::new(api).synthesize(playback).unwrap();
    let db = RawDb::open(&db_path_for(out_db)).await.unwrap();
    let download = fetch(FetchOptions {
        sleep_between: Duration::ZERO,
        conv_uuids: vec![CONV.to_string()],
        ..FetchOptions::new(db.clone())
    });
    let summary = playback::scope(playback, download).await;
    db.commit_all("test").await.unwrap();
    let stored = db
        .load_conversation_payload(CONV)
        .await
        .unwrap()
        .expect("the conversation is stored");
    db.close().await;
    let summary = summary.unwrap();
    assert_eq!((summary.fetched, summary.errors), (1, 0));
    stored
}

/// OpenAI ships a model and every assistant message's
/// `model_switcher_deny` grows, with the conversation's `update_time`
/// unmoved. A re-fetch (a named conversation, a reset, a widened
/// config) must leave the content row as it was, or the render's diff
/// reads the conversation as changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_re_fetch_that_moves_only_model_switcher_deny_leaves_the_row_alone() {
    let d = tempdir().unwrap();
    let api = d.path().join("api");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out.doltlite_db");
    write_json(
        &api.join("me.json"),
        &json!({"id": "u-1", "email": "geordi@enterprise.test"}),
    );
    write_json(&api.join("conversations.json"), &json!([]));

    write_json(
        &api.join(format!("conversations/{CONV}.json")),
        &detail(&["soong-3"]),
    );
    let first = sync(&api, &playback, &out_db).await;

    let newer = detail(&["soong-3", "soong-5", "lore-1"]);
    write_json(&api.join(format!("conversations/{CONV}.json")), &newer);
    let second = sync(&api, &playback, &out_db).await;

    assert_eq!(
        first, second,
        "only the switcher's list moved, so the content row must not"
    );
    assert!(
        second
            .pointer("/mapping/a1/message/metadata/model_switcher_deny")
            .is_none(),
        "the list lives in the sidecar, not the content row"
    );
    assert_eq!(
        second.pointer("/mapping/a1/message/metadata/model_slug"),
        Some(&json!("soong-4")),
        "the rest of the message metadata stays content"
    );

    let loaded = block_on_load_all(&db_path_for(&out_db)).unwrap();
    assert_eq!(
        loaded.conversations[0].payload, newer,
        "the loader puts the newest list back, so the wire object round-trips"
    );
}
