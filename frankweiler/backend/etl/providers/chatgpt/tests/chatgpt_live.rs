// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live ChatGPT single-conversation download test.
//!
//! Hits real `chatgpt.com/backend-api` via `latchkey curl` (and the
//! `latchkey-curl-shim` Rust shim that injects a Chrome TLS
//! fingerprint), downloads ONE known conversation into a hermetic
//! tempdir, and insta-snapshots a curated stable view. Documents the
//! end-to-end live shape.
//!
//! Tagged `manual` in Bazel and `#[ignore]` in cargo; run with:
//!
//! ```sh
//! export LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim
//! cargo test -p frankweiler-etl-chatgpt --test chatgpt_live -- --ignored
//! ```
//!
//! The target conversation is tied to the test author's chatgpt.com
//! account; accept any title/content changes via `cargo insta review`.

use std::time::Duration;

use frankweiler_etl_chatgpt::download::{self as chatgpt, db::block_on_load_all, db::db_path_for};
use insta::assert_json_snapshot;
use serde_json::{json, Value};

const TARGET_ID: &str = "69b446c9-f0a0-832f-b9c2-5ccaaf3f108d";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn chatgpt_live_single_conv_snapshot() {
    let tmp = tempfile::TempDir::with_prefix("chatgpt-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] downloading to {}", tmp.display());

    let opts = chatgpt::FetchOptions {
        db_path: tmp.clone(),
        max_pages: None,
        limit: None,
        sleep_between: Duration::ZERO,
        conv_uuids: vec![TARGET_ID.to_string()],
        ..Default::default()
    };
    chatgpt::fetch(opts).await.expect("chatgpt fetch failed");

    let db_path = db_path_for(&tmp);
    let raw = block_on_load_all(&db_path).expect("load db");
    let conv: Value = raw
        .conversations
        .into_iter()
        .find(|c| c.id == TARGET_ID)
        .expect("conv present in db")
        .payload;

    // Walk the mapping in create_time order so the snapshot doesn't
    // depend on hashmap iteration order. Skip nodes with no message
    // (root node has `message: null`).
    let mut nodes: Vec<&Value> = conv
        .get("mapping")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.values()
                .filter(|n| n.get("message").is_some() && !n.get("message").unwrap().is_null())
                .collect()
        })
        .unwrap_or_default();
    nodes.sort_by(|a, b| {
        let ka = a
            .pointer("/message/create_time")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::INFINITY);
        let kb = b
            .pointer("/message/create_time")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::INFINITY);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });

    let messages: Vec<Value> = nodes
        .iter()
        .map(|n| {
            let m = n.get("message").unwrap();
            let role = m.pointer("/author/role").and_then(|v| v.as_str());
            let content_type = m.pointer("/content/content_type").and_then(|v| v.as_str());
            let parts = m
                .pointer("/content/parts")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let text_chars: usize = m
                .pointer("/content/parts")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| p.as_str())
                        .map(|s| s.chars().count())
                        .sum()
                })
                .unwrap_or(0);
            json!({
                "role": role,
                "content_type": content_type,
                "parts": parts,
                "text_chars": text_chars,
            })
        })
        .collect();

    let view = json!({
        "id": conv.get("conversation_id").or_else(|| conv.get("id")),
        "title": conv.get("title"),
        "default_model_slug": conv.get("default_model_slug"),
        "mapping_size": conv.get("mapping").and_then(|v| v.as_object()).map(|m| m.len()).unwrap_or(0),
        "message_count": messages.len(),
        "messages": messages,
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!(view);
    });
}
