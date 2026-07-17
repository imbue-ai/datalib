// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Anthropic single-conversation download test.
//!
//! Hits real `claude.ai/api` via `latchkey curl` (and the
//! `latchkey-curl-shim` Rust shim that injects a Chrome TLS
//! fingerprint), downloads ONE known conversation into a hermetic
//! tempdir, and insta-snapshots a curated stable view of what came
//! back. Serves as both an integration smoke test and a piece of
//! documentation showing what a Claude export looks like end-to-end.
//!
//! Tagged `manual` in Bazel and `#[ignore]` in cargo; run with:
//!
//! ```sh
//! export LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim
//! cargo test -p frankweiler-etl-anthropic --test anthropic_live -- --ignored
//! ```
//!
//! The target conversation is real and tied to the test author's
//! claude.ai account; if its title/content changes, accept the new
//! snapshot via `cargo insta review`.

use std::time::Duration;

use frankweiler_etl_anthropic::download::{
    self as anthropic, db::block_on_load_all, db::db_path_for,
};
use insta::assert_json_snapshot;
use serde_json::{json, Value};

const TARGET_UUID: &str = "b0c2f022-cc28-4888-b038-702ec040b87b";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn anthropic_live_single_conv_snapshot() {
    let tmp = tempfile::TempDir::with_prefix("anthropic-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] downloading to {}", tmp.display());

    let opts = anthropic::FetchOptions {
        db_path: tmp.clone(),
        export_dir: None,
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: vec![TARGET_UUID.to_string()],
        ..Default::default()
    };
    anthropic::fetch(opts)
        .await
        .expect("anthropic fetch failed");

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
