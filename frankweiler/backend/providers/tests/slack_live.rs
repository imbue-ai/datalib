//! Live Slack integration snapshot test.
//!
//! Hits real api.slack.com via `latchkey curl`, downloads the configured
//! test channel, and snapshots each entity's `updated/` JSONL stream with
//! redactions for volatile fields. Marked `#[ignore]` so `cargo test`
//! skips it; run explicitly with:
//!
//!     cargo test -p frankweiler-providers --test slack_live -- --ignored
//!
//! Snapshots live in `providers/tests/snapshots/`. Accept changes with
//! `cargo insta review` after posting messages / attachments to the
//! channel.
//!
//! Prereq: `latchkey` on PATH with creds for the `slack` service.

use std::fs;
use std::path::Path;

use frankweiler_providers::slack;
use insta::assert_json_snapshot;
use serde_json::Value;

const TEST_CHANNEL: &str = "thad-testing-channel";

fn load_jsonl_sorted(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut rows: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid jsonl"))
        .collect();
    // Order varies with API pagination. Sort by the canonical JSON form of
    // each record's key fields so snapshots are stable across runs.
    rows.sort_by_key(|r| {
        let mut k = String::new();
        for field in [
            "channel_id",
            "user_id",
            "thread_ts",
            "message_ts",
            "reply_ts",
        ] {
            if let Some(v) = r.get(field).and_then(|v| v.as_str()) {
                k.push_str(v);
                k.push('\x1f');
            }
        }
        k
    });
    rows
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

    // Authors referenced by our test channel's traffic — used to trim the
    // user snapshot down to just the users we care about, so the rest of
    // the workspace's directory doesn't churn the snapshot.
    let mut referenced_users: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for entity in ["message", "reply"] {
        let path = tmp.join(entity).join("updated").join("events.jsonl");
        for r in load_jsonl_sorted(&path) {
            if let Some(u) = r
                .get("raw")
                .and_then(|raw| raw.get("user"))
                .and_then(|v| v.as_str())
            {
                referenced_users.insert(u.to_string());
            }
        }
    }

    // Redact volatile fields: _recorded_at is wall clock; url_private*,
    // permalink*, thumb_* are signed Slack URLs regenerated each call.
    for entity in [
        "self_identity",
        "channel",
        "user",
        "message",
        "reply",
        "reaction",
    ] {
        let path = tmp.join(entity).join("updated").join("events.jsonl");
        let mut rows = load_jsonl_sorted(&path);
        // Trim noisy workspace-wide listings down to what's relevant to
        // the test channel. Whole-workspace churn isn't what we're testing.
        if entity == "channel" {
            rows.retain(|r| r.get("channel_name").and_then(|v| v.as_str()) == Some(TEST_CHANNEL));
        } else if entity == "user" {
            rows.retain(|r| {
                r.get("user_id")
                    .and_then(|v| v.as_str())
                    .map(|u| referenced_users.contains(u))
                    .unwrap_or(false)
            });
        }
        insta::with_settings!({
            snapshot_suffix => entity,
            sort_maps => true,
        }, {
            assert_json_snapshot!(rows, {
                "[]._recorded_at" => "[ts]",
                "[].raw.url_private" => "[url]",
                "[].raw.url_private_download" => "[url]",
                "[].raw.permalink" => "[url]",
                "[].raw.permalink_public" => "[url]",
                "[].raw.files[].url_private" => "[url]",
                "[].raw.files[].url_private_download" => "[url]",
                "[].raw.files[].permalink" => "[url]",
                "[].raw.files[].permalink_public" => "[url]",
                "[].raw.files[].thumb_64" => "[url]",
                "[].raw.files[].thumb_80" => "[url]",
                "[].raw.files[].thumb_160" => "[url]",
                "[].raw.files[].thumb_360" => "[url]",
                "[].raw.files[].thumb_480" => "[url]",
                "[].raw.files[].thumb_720" => "[url]",
                "[].raw.files[].thumb_800" => "[url]",
                "[].raw.files[].thumb_960" => "[url]",
                "[].raw.files[].thumb_1024" => "[url]",
                "[].raw.files[].thumb_pdf" => "[url]",
                "[].raw.files[].thumb_video" => "[url]",
            });
        });
    }
}
