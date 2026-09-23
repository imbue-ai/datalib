//! A stop requested mid-run ends the walk at the next channel boundary:
//! the channel in flight finishes, no further channel starts, and the run
//! returns `Ok` with what it has — the shape `finish` then commits as the
//! last seal. The flag is raised from the progress sink's per-channel
//! tick, so the test does not depend on timing.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use datalib_etl::control::DownloadControl;
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_slack::ingest::{block_on_load_all, db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

const TS_SINCE: &str = "1704067200.000000";

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

fn write_fixture(api: &Path, channels: &[&str]) {
    write_envelope(
        &api.join("raw_api/auth.test/run-1.jsonl"),
        &json!({
            "method": "auth.test", "params": {},
            "response": {"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"},
        }),
    );
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {"ok": true, "members": [
                {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
            ]},
        }),
    );
    let listed: Vec<Value> = channels
        .iter()
        .map(
            |c| json!({"id": c, "name": c.to_lowercase(), "is_member": true, "is_archived": false}),
        )
        .collect();
    write_envelope(
        &api.join("raw_api/conversations.list/run-1.jsonl"),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": "public_channel,private_channel",
            },
            "response": {"ok": true, "channels": listed, "has_more": false},
        }),
    );
    for (i, c) in channels.iter().enumerate() {
        write_envelope(
            &api.join(format!("raw_api/conversations.history/{c}.jsonl")),
            &json!({
                "method": "conversations.history",
                "params": {
                    "channel": c,
                    "include_all_metadata": "true",
                    "inclusive": "true",
                    "limit": "200",
                    "oldest": TS_SINCE,
                },
                "response": {
                    "ok": true,
                    "messages": [{"ts": format!("1735689600.0001{i:02}"), "user": "U1", "text": format!("in {c}")}],
                    "has_more": false,
                },
            }),
        );
    }
}

/// Raises the stop on the download's first progress tick — what the SIGINT
/// handler does, at a point the test controls. The first tick now lands
/// part-way through the first channel rather than at its end, so this
/// asserts the stronger thing: the channel already in flight still
/// finishes, and none after it starts.
struct StopOnFirstChannel(StopFlag);

impl ProgressSink for StopOnFirstChannel {
    fn inc(&self, _delta: u64) {
        self.0.request();
    }
}

fn channels_with_messages(out: &Path) -> BTreeSet<String> {
    let raw = block_on_load_all(&db_path_for(out)).expect("load db");
    raw.messages.iter().map(|m| m.channel_id.clone()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_ends_the_walk_at_the_next_channel_boundary() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");
    write_fixture(&api, &["C1", "C2", "C3"]);
    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    let stop = StopFlag::new();
    let db = RawDb::open(&db_path_for(&out)).await.unwrap();
    let summary = fetch(FetchOptions {
        channels: None,
        since: "2024-01-01".into(),
        refresh_window_days: 0,
        members_only: false,
        media: false,
        dms: false,
        dm_conversations: None,
        progress: Progress::new(Arc::new(StopOnFirstChannel(stop.clone()))),
        control: DownloadControl {
            stop: stop.clone(),
            ..Default::default()
        },
        ..FetchOptions::new(db.clone())
    })
    .await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    summary.expect("a stopped run is a shorter run, not a failed one");

    let walked = channels_with_messages(&out);
    assert_eq!(
        walked.len(),
        1,
        "one channel finished, none started after the stop: {walked:?}"
    );
    assert!(stop.requested());

    // The run did not walk every channel the config names, so it must not
    // have recorded the config as satisfied: the next run has to reach
    // the channels this one never started, exactly as after a widened
    // filter.
    let reader = datalib_pin::open_reader(&db_path_for(&out)).await.unwrap();
    let recorded = datalib_etl::scope_config::load(&reader, "slack:download")
        .await
        .unwrap();
    reader.close().await;
    assert!(
        recorded.is_none(),
        "an interrupted run recorded its scope config as satisfied: {recorded:?}"
    );
}
