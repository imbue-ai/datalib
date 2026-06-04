// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Slack integration snapshot test.
//!
//! Hits real api.slack.com via `latchkey curl`, downloads the configured
//! test channel into a doltlite DB, and snapshots the captured rows
//! per-table with redactions for volatile fields. Marked `#[ignore]`
//! so `cargo test` skips it; run explicitly with:
//!
//!     cargo test -p frankweiler-etl-slack --test slack_live -- --ignored
//!
//! Snapshots live in `tests/snapshots/`. Accept changes with
//! `cargo insta review` (or the bazel `.update` target) after posting
//! messages / attachments to the channel.
//!
//! Prereq: `latchkey` on PATH with creds for the `slack` service.

use frankweiler_etl_slack::extract::{self as slack, block_on_load_all, db_path_for};
use insta::assert_json_snapshot;
use serde_json::Value;

const TEST_CHANNEL: &str = "thad-testing-channel";

/// Sort a Vec of JSON payloads by a sequence of string keys, for
/// snapshot stability across pagination/cursor reordering.
fn sort_by_keys(mut items: Vec<Value>, sort_keys: &[&str]) -> Vec<Value> {
    items.sort_by_key(|v| {
        let mut k = String::new();
        for f in sort_keys {
            if let Some(s) = v.get(*f).and_then(|x| x.as_str()) {
                k.push_str(s);
                k.push('\x1f');
            }
        }
        k
    });
    items
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn slack_live_download_snapshot() {
    let tmp = tempfile::TempDir::with_prefix("slack-live-")
        .expect("create tempdir")
        .keep();

    eprintln!("[test] downloading to {}", tmp.display());

    let opts = slack::FetchOptions {
        db_path: tmp.clone(),
        channels: Some(vec![TEST_CHANNEL.to_string()]),
        media: true,
        ..Default::default()
    };
    slack::fetch(opts).await.expect("slack fetch failed");

    let db_path = db_path_for(&tmp);
    let raw = block_on_load_all(&db_path).expect("load db");

    // Trim conversations to the test channel — workspace-wide listings
    // include every channel the token can see, and that churn isn't
    // what we're snapshotting.
    let mut channels: Vec<Value> = raw
        .channels
        .into_iter()
        .filter(|c| c.get("name").and_then(|v| v.as_str()) == Some(TEST_CHANNEL))
        .collect();
    channels = sort_by_keys(channels, &["id"]);

    // Messages: split into top-level vs thread replies the same way
    // the old JSONL-tree assertions did. `is_thread_root && thread_ts
    // == None` → channel-history-only; otherwise either a thread root
    // re-served by replies, or a reply child.
    let messages: Vec<Value> = raw
        .messages
        .iter()
        .filter(|m| m.channel_id == channels[0].get("id").and_then(|v| v.as_str()).unwrap_or(""))
        .filter(|m| m.thread_ts.is_none())
        .map(|m| m.payload.clone())
        .collect();
    let messages = sort_by_keys(messages, &["ts"]);

    let replies: Vec<Value> = raw
        .messages
        .iter()
        .filter(|m| m.channel_id == channels[0].get("id").and_then(|v| v.as_str()).unwrap_or(""))
        .filter(|m| m.thread_ts.is_some())
        .map(|m| m.payload.clone())
        .collect();
    let replies = sort_by_keys(replies, &["thread_ts", "ts"]);

    // Users: trim to authors referenced by the test channel's traffic.
    let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for m in messages.iter().chain(replies.iter()) {
        if let Some(u) = m.get("user").and_then(|v| v.as_str()) {
            referenced.insert(u.to_string());
        }
    }
    let mut users: Vec<Value> = raw
        .users
        .into_iter()
        .filter(|u| {
            u.get("id")
                .and_then(|v| v.as_str())
                .map(|id| referenced.contains(id))
                .unwrap_or(false)
        })
        .collect();
    users = sort_by_keys(users, &["id"]);

    // Redact volatile fields: url_private*, permalink*, thumb_* are
    // signed Slack URLs regenerated each call.
    for (suffix, value) in [
        ("channels", &channels),
        ("users", &users),
        ("messages", &messages),
        ("replies", &replies),
    ] {
        insta::with_settings!({
            snapshot_suffix => suffix,
            sort_maps => true,
        }, {
            assert_json_snapshot!(value, {
                "[].url_private" => "[url]",
                "[].url_private_download" => "[url]",
                "[].permalink" => "[url]",
                "[].permalink_public" => "[url]",
                "[].files[].url_private" => "[url]",
                "[].files[].url_private_download" => "[url]",
                "[].files[].permalink" => "[url]",
                "[].files[].permalink_public" => "[url]",
                "[].files[].thumb_64" => "[url]",
                "[].files[].thumb_80" => "[url]",
                "[].files[].thumb_160" => "[url]",
                "[].files[].thumb_360" => "[url]",
                "[].files[].thumb_480" => "[url]",
                "[].files[].thumb_720" => "[url]",
                "[].files[].thumb_800" => "[url]",
                "[].files[].thumb_960" => "[url]",
                "[].files[].thumb_1024" => "[url]",
                "[].files[].thumb_pdf" => "[url]",
                "[].files[].thumb_video" => "[url]",
            });
        });
    }
}
