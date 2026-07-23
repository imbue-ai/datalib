//! Slack synth → playback → download round-trip.
//!
//! Uses the in-tree synthesizer to turn JSONL envelope fixtures into
//! HTTP playback bodies, then runs download against the playback root.
//! Asserts on the populated doltlite DB rather than on a JSONL
//! disk tree — the doltlite store is the entire output of the
//! download stage post-port.

use std::fs;
use std::path::Path;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_slack::download::{block_on_load_all, db_path_for, fetch, FetchOptions};
use frankweiler_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
            "response": {"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"},
        }),
    );

    // conversations.list — exact param shape download emits.
    write_envelope(
        &api.join("raw_api/conversations.list/run-1.jsonl"),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": "public_channel,private_channel,im,mpim",
            },
            "response": {
                "ok": true,
                "channels": [
                    {"id": "C1", "name": "general", "is_member": true},
                    {"id": "D1", "is_im": true, "user": "U2"},
                    {
                        "id": "G1",
                        "name": "mpdm-alice--bob--carol-1",
                        "is_mpim": true,
                        "members": ["U1", "U2", "U3"],
                    },
                ],
            },
        }),
    );

    // users.list
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {
                "ok": true,
                "members": [
                    {"id": "U1", "name": "alice"},
                    {"id": "U2", "name": "bob"},
                    {"id": "U3", "name": "carol"},
                ],
            },
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

    // One-to-one DM — Slack IM objects commonly omit `name` and
    // `is_member`; direct_messages=true must still plan the history fetch.
    write_envelope(
        &api.join("raw_api/conversations.history/run-2.jsonl"),
        &json!({
            "method": "conversations.history",
            "params": {
                "channel": "D1",
                "include_all_metadata": "true",
                "inclusive": "true",
                "limit": "200",
                "oldest": "1704067200.000000",
            },
            "response": {
                "ok": true,
                "messages": [{
                    "ts": "1735689601.000000",
                    "user": "U2",
                    "text": "Synthetic direct message",
                }],
                "has_more": false,
            },
        }),
    );

    // Multi-person DM, also without `is_member`.
    write_envelope(
        &api.join("raw_api/conversations.history/run-3.jsonl"),
        &json!({
            "method": "conversations.history",
            "params": {
                "channel": "G1",
                "include_all_metadata": "true",
                "inclusive": "true",
                "limit": "200",
                "oldest": "1704067200.000000",
            },
            "response": {
                "ok": true,
                "messages": [{
                    "ts": "1735689602.000000",
                    "user": "U3",
                    "text": "Synthetic group direct message",
                }],
                "has_more": false,
            },
        }),
    );

    let report = SlackSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(report.fixtures_written, 6);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        db_path: out.clone(),
        channels: None,
        direct_messages: true,
        since: "2024-01-01".into(),
        refresh_window_days: 0,
        members_only: true,
        media: false,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.messages, 3);

    // Inspect the resulting doltlite DB: one workspace, three conversation
    // types, three users, and three messages — all sourced from playback
    // verbatim.
    let db_path = db_path_for(&out);
    assert!(db_path.exists(), "expected DB at {}", db_path.display());
    let raw = block_on_load_all(&db_path).expect("load db");
    let ws = raw.workspace.expect("workspace");
    assert_eq!(ws["team_id"], "T1");
    assert_eq!(raw.users.len(), 3);
    assert_eq!(raw.channels.len(), 3);
    assert_eq!(raw.messages.len(), 3);
    assert!(raw
        .messages
        .iter()
        .any(|m| m.channel_id == "C1" && m.payload["text"] == "hello"));
    assert!(raw
        .messages
        .iter()
        .any(|m| m.channel_id == "D1" && m.payload["text"] == "Synthetic direct message"));
    assert!(raw
        .messages
        .iter()
        .any(|m| m.channel_id == "G1" && m.payload["text"] == "Synthetic group direct message"));
}
