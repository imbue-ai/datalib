//! Slack synth → playback → extract round-trip.

use std::fs;
use std::path::Path;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_slack::extract::{fetch, FetchOptions};
use frankweiler_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn slack_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    // auth.test
    write_envelope(
        &api.join("raw_api/auth.test/run-1.jsonl"),
        &json!({
            "method": "auth.test", "params": {},
            "response": {"ok": true, "user_id": "U1", "team": "T1"},
        }),
    );

    // conversations.list — exact param shape extract emits.
    write_envelope(
        &api.join("raw_api/conversations.list/run-1.jsonl"),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": "public_channel,private_channel",
            },
            "response": {
                "ok": true,
                "channels": [{"id": "C1", "name": "general", "is_member": true}],
            },
        }),
    );

    // users.list
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {"ok": true, "members": [{"id": "U1", "name": "alice"}]},
        }),
    );

    // conversations.history — channel=C1, oldest = slack-ts of 2024-01-01.
    // datetime_to_slack_ts(2024-01-01T00:00:00Z) → "1704067200.000000".
    write_envelope(
        &api.join("raw_api/conversations.history/run-1.jsonl"),
        &json!({
            "method": "conversations.history",
            "params": {
                "channel": "C1",
                "include_all_metadata": "true",
                "inclusive": "true",
                "limit": "200",
                "oldest": "1704067200.000000",
            },
            "response": {
                "ok": true,
                "messages": [{"ts": "1735689600.000000", "user": "U1", "text": "hello"}],
                "has_more": false,
            },
        }),
    );

    let report = SlackSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(report.fixtures_written, 4);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        out_dir: out.clone(),
        channels: None,
        since: "2024-01-01".into(),
        refresh_window_days: 0,
        members_only: false,
        media: false,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.messages, 1);

    // The new raw_api tree should contain identical responses for every
    // method, since playback hands extract back exactly what synth read.
    for method in [
        "auth.test",
        "conversations.list",
        "users.list",
        "conversations.history",
    ] {
        let want = read_responses(&api, method);
        let got = read_responses(&out, method);
        assert_eq!(got, want, "{method} mismatch");
    }
}

fn read_responses(root: &Path, method: &str) -> Vec<Value> {
    let dir = root.join("raw_api").join(method);
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    for f in files {
        for line in fs::read_to_string(&f).unwrap().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap();
            out.push(v.get("response").cloned().unwrap_or(Value::Null));
        }
    }
    out
}
