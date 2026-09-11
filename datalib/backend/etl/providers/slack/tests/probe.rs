//! The Slack probe end to end against a playback workspace: the same
//! three listing calls the downloader makes, replayed from fixtures,
//! and the report the wizard's pickers are filled from.

use std::fs;
use std::path::Path;

use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_slack::probe::probe;
use datalib_etl_slack::synthesize::SlackSynth;
use datalib_etl_slack_config::SlackConfig;
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

fn write_workspace(api: &Path) {
    write_envelope(
        &api.join("raw_api/auth.test/run-1.jsonl"),
        &json!({
            "method": "auth.test", "params": {},
            "response": {
                "ok": true, "user": "picard", "user_id": "U1",
                "team": "Enterprise", "team_id": "T1",
            },
        }),
    );
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {"ok": true, "members": [
                {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
                {"id": "U2", "name": "riker", "real_name": "William Riker"},
                {"id": "U3", "name": "data", "real_name": "Data"},
            ]},
        }),
    );
    // The probe asks for every kind at once, exactly as a `dms = true`
    // download does, so this is the one listing it needs.
    write_envelope(
        &api.join("raw_api/conversations.list/run-1.jsonl"),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": "public_channel,private_channel,im,mpim",
            },
            "response": {"ok": true, "channels": [
                {"id": "C1", "name": "general", "is_member": true, "num_members": 3},
                {"id": "C2", "name": "warp-core", "is_member": true, "is_private": true},
                {"id": "C3", "name": "holodeck", "is_member": false},
                {"id": "D1", "is_im": true, "user": "U2"},
                {"id": "G1", "is_mpim": true, "members": ["U1", "U2", "U3"]},
            ]},
        }),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_lists_channels_and_the_people_behind_dms() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    write_workspace(&api);
    let synth = SlackSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(synth.fixtures_written, 3);
    std::env::set_var(PLAYBACK_ENV, &playback);

    let config: SlackConfig = serde_json::from_value(json!({"api": {}})).unwrap();
    let report = probe(&config).await.unwrap();

    assert_eq!(report.mode, "api");
    assert_eq!(report.account.id, "U1");
    assert_eq!(
        report.account.display_name.as_deref(),
        Some("picard in Enterprise")
    );

    let rows: Vec<(String, String, Option<String>, Option<String>)> = report
        .items
        .iter()
        .map(|i| {
            (
                i.kind.as_str().to_string(),
                i.path.clone(),
                i.title.clone(),
                i.role.clone(),
            )
        })
        .collect();
    let s = |v: &str| Some(v.to_string());
    assert_eq!(
        rows,
        vec![
            ("channel".into(), "general".into(), None, None),
            ("channel".into(), "warp-core".into(), None, s("private")),
            ("channel".into(), "holodeck".into(), None, s("not a member")),
            ("person".into(), "U3".into(), s("Data"), s("@data")),
            (
                "person".into(),
                "U2".into(),
                s("William Riker"),
                s("@riker")
            ),
        ]
    );
    assert_eq!(report.items[0].members, Some(3));
}
