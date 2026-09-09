// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Claude single-conversation download test.

use std::time::Duration;

use datalib_etl_claude::download::{self as claude, db::block_on_load_all, db::db_path_for};
use insta::assert_json_snapshot;
use serde_json::{json, Value};

const TARGET_UUID: &str = "b0c2f022-cc28-4888-b038-702ec040b87b";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn claude_live_single_conv_snapshot() {
    let tmp = tempfile::TempDir::with_prefix("claude-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] downloading to {}", tmp.display());

    let db = claude::RawDb::open(&claude::db_path_for(&tmp))
        .await
        .unwrap();
    let opts = claude::FetchOptions {
        export_dir: None,
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: vec![TARGET_UUID.to_string()],
        ..claude::FetchOptions::new(db.clone())
    };
    let r = claude::fetch(opts).await;
    db.close().await;
    r.expect("claude fetch failed");

    let raw = block_on_load_all(&db_path_for(&tmp)).expect("load db");
    let conv = raw
        .conversations
        .iter()
        .find(|c| c.id == TARGET_UUID)
        .expect("target conversation present in db")
        .payload
        .clone();
    let conv = &conv;

    let messages: Vec<Value> = conv
        .get("chat_messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let kinds: Vec<String> = m
                .get("content")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|b| b.get("type").and_then(|t| t.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let text_chars = m
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().count())
                .unwrap_or(0);
            json!({
                "sender": m.get("sender"),
                "block_kinds": kinds,
                "text_chars": text_chars,
                "attachments": m.get("attachments").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                "files": m.get("files").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
            })
        })
        .collect();

    let view = json!({
        "uuid": conv.get("uuid"),
        "name": conv.get("name"),
        "model": conv.get("model"),
        "message_count": messages.len(),
        "messages": messages,
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!(view);
    });
}
