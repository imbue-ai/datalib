//! The Slack probe end to end against a playback workspace: the
//! listing calls the downloader makes, replayed from fixtures, and the
//! reports the wizard's check and pickers are filled from.

use std::path::Path;

use datalib_etl_slack::probe::probe;
use datalib_etl_slack::recorded::record_call;
use datalib_etl_slack::synthesize::SlackSynth;
use datalib_etl_slack_config::SlackConfig;
use datalib_etl_web::http::PLAYBACK_ENV;
use datalib_etl_web::synthesize::Synthesizer;
use datalib_probe::{ProbeAsk, ProbeList, ProbeProgress, ProbeReport};
use serde_json::{json, Value};
use std::sync::Mutex;
use tempfile::tempdir;

fn write_workspace(api: &Path) {
    let call = |method: &str, params: Value, response: Value| {
        record_call(api, method, params, response).unwrap();
    };
    call(
        "auth.test",
        json!({}),
        json!({
            "ok": true, "user": "picard", "user_id": "U1",
            "team": "Enterprise", "team_id": "T1",
        }),
    );
    call(
        "users.list",
        json!({"limit": "200"}),
        json!({"ok": true, "members": [
            {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
            {"id": "U2", "name": "riker", "real_name": "William Riker"},
            {"id": "U3", "name": "data", "real_name": "Data"},
        ]}),
    );
    // One listing per picker: channels for the channel picker, DMs
    // for the DM picker, so neither pays for the other.
    call(
        "conversations.list",
        json!({
            "exclude_archived": "true",
            "limit": "200",
            "types": "public_channel,private_channel",
        }),
        json!({"ok": true, "channels": [
            {"id": "C1", "name": "general", "is_member": true, "num_members": 3},
            {"id": "C2", "name": "warp-core", "is_member": true, "is_private": true},
            {"id": "C3", "name": "holodeck", "is_member": false},
        ]}),
    );
    call(
        "conversations.list",
        json!({"exclude_archived": "true", "limit": "200", "types": "im,mpim"}),
        json!({"ok": true, "channels": [
            {"id": "D1", "is_im": true, "user": "U2"},
            {"id": "G1", "is_mpim": true, "members": ["U1", "U2", "U3"]},
        ]}),
    );
}

type Row<'a> = (&'a str, &'a str, &'a str, &'a str);

fn rows(report: &ProbeReport) -> Vec<Row<'_>> {
    report
        .items
        .iter()
        .map(|i| {
            (
                i.kind.as_str(),
                i.path.as_str(),
                i.title.as_deref().unwrap_or(""),
                i.role.as_deref().unwrap_or(""),
            )
        })
        .collect()
}

/// Each ask against one playback workspace: the account alone, then
/// each picker's list, with the progress each reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_ask_reads_only_what_it_needs() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    write_workspace(&api);
    let synth = SlackSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(synth.fixtures_written, 4);
    std::env::set_var(PLAYBACK_ENV, &playback);
    let config: SlackConfig = serde_json::from_value(json!({"api": {}})).unwrap();

    let seen = Mutex::new(Vec::new());
    let record = |p: ProbeProgress| seen.lock().unwrap().push(p.done);

    let account = probe(&config, ProbeAsk::Account, &record).await.unwrap();
    assert_eq!(account.mode, "api");
    assert_eq!(account.account.id, "U1");
    assert_eq!(
        account.account.display_name.as_deref(),
        Some("picard in Enterprise")
    );
    assert!(account.items.is_empty());
    assert!(seen.lock().unwrap().is_empty(), "a check lists nothing");

    let channels = probe(&config, ProbeAsk::List(ProbeList::Channels), &record)
        .await
        .unwrap();
    assert_eq!(
        rows(&channels),
        vec![
            ("channel", "general", "", ""),
            ("channel", "warp-core", "", "private"),
            ("channel", "holodeck", "", "not a member"),
        ]
    );
    assert_eq!(channels.items[0].members, Some(3));
    assert_eq!(std::mem::take(&mut *seen.lock().unwrap()), vec![3]);

    // A DM's path is the id `dm_conversations` takes, and its title is
    // what the sync will call it.
    let dms = probe(&config, ProbeAsk::List(ProbeList::Conversations), &record)
        .await
        .unwrap();
    assert_eq!(
        rows(&dms),
        vec![
            ("conversation", "D1", "@William Riker", ""),
            ("conversation", "G1", "@William Riker, Data", "group"),
        ]
    );
    // The directory and the DMs count as one list.
    assert_eq!(*seen.lock().unwrap(), vec![3, 5]);

    let err = probe(&config, ProbeAsk::List(ProbeList::Labels), &record)
        .await
        .expect_err("Slack has no labels");
    assert!(err.to_string().contains("no `labels` list"), "{err}");
}
