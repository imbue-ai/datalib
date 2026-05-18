//! Live Slack integration snapshot test.
//!
//! Hits real api.slack.com via `latchkey curl`, downloads the configured
//! test channel, and snapshots each captured `raw_api/<method>` stream
//! with redactions for volatile fields. Marked `#[ignore]` so
//! `cargo test` skips it; run explicitly with:
//!
//!     cargo test -p frankweiler-etl --test slack_live -- --ignored
//!
//! Snapshots live in `providers/tests/snapshots/`. Accept changes with
//! `cargo insta review` after posting messages / attachments to the
//! channel.
//!
//! Prereq: `latchkey` on PATH with creds for the `slack` service.

use std::fs;
use std::path::Path;

use frankweiler_etl_slack::extract as slack;
use insta::assert_json_snapshot;
use serde_json::Value;

const TEST_CHANNEL: &str = "thad-testing-channel";

/// Fan-in every `*.jsonl` under a method directory. Run-stamped files
/// sort lexically by timestamp prefix, so iteration order is run order.
fn load_jsonl(method_dir: &Path) -> Vec<Value> {
    let mut paths: Vec<_> = fs::read_dir(method_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let text = fs::read_to_string(&p).unwrap_or_default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            out.push(serde_json::from_str(line).expect("valid jsonl"));
        }
    }
    out
}

/// Pull only the items inside each page envelope. Sorting keeps the
/// snapshot stable across pagination/cursor reordering, and the page
/// envelope metadata (`_recorded_at`, `params.cursor`) is volatile in
/// ways that aren't worth redacting per-field.
fn extract_items(rows: &[Value], array_field: &str, sort_keys: &[&str]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for row in rows {
        if let Some(arr) = row
            .get("response")
            .and_then(|r| r.get(array_field))
            .and_then(|v| v.as_array())
        {
            items.extend(arr.iter().cloned());
        }
    }
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
    let tmp = std::env::temp_dir().join(format!(
        "slack-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    eprintln!("[test] downloading to {}", tmp.display());

    let opts = slack::FetchOptions {
        out_dir: tmp.clone(),
        channels: Some(vec![TEST_CHANNEL.to_string()]),
        media: true,
        ..Default::default()
    };
    slack::fetch(opts).await.expect("slack fetch failed");

    let raw = tmp.join("raw_api");

    // Trim conversations.list down to the test channel — workspace-wide
    // churn isn't what we're testing.
    let channels_rows = load_jsonl(&raw.join("conversations.list"));
    let mut channels = extract_items(&channels_rows, "channels", &["id"]);
    channels.retain(|c| c.get("name").and_then(|v| v.as_str()) == Some(TEST_CHANNEL));

    // Messages + replies — full sets are tiny for the test channel.
    let history_rows = load_jsonl(&raw.join("conversations.history"));
    let messages = extract_items(&history_rows, "messages", &["ts"]);
    let replies_rows = load_jsonl(&raw.join("conversations.replies"));
    let replies = extract_items(&replies_rows, "messages", &["thread_ts", "ts"]);

    // Users: trim to authors referenced by the test channel's traffic.
    let mut referenced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for m in messages.iter().chain(replies.iter()) {
        if let Some(u) = m.get("user").and_then(|v| v.as_str()) {
            referenced.insert(u.to_string());
        }
    }
    let users_rows = load_jsonl(&raw.join("users.list"));
    let mut users = extract_items(&users_rows, "members", &["id"]);
    users.retain(|u| {
        u.get("id")
            .and_then(|v| v.as_str())
            .map(|id| referenced.contains(id))
            .unwrap_or(false)
    });

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
