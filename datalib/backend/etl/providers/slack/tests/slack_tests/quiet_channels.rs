//! #1423: a routine sync asked every channel's history for what was
//! newer, one request each, so 126 quiet channels cost 80 s. A channel
//! `client.counts` shows with nothing newer than what is held now costs
//! no request, and the refresh window is re-read at most once per
//! `refresh_every_minutes`. Every page a walk could ask for is served,
//! so a channel walked when it should not be lands its message and fails
//! the assertion.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_slack::ingest::{block_on_load_all, db_path_for, FetchOptions};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};

use crate::support::{fetch_into, msg, serve, stored_ts};

/// 2025-01-01: newer than the default `since`.
const T1: &str = "1735689600.000100";
const T2: &str = "1735689600.000200";

fn record_listing(api: &Path, channels: &[&str]) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    let listed: Vec<Value> = channels
        .iter()
        .map(|c| json!({"id": c, "name": c.to_lowercase(), "is_member": true}))
        .collect();
    record_conversations(api, CHANNEL_TYPES, json!(listed)).unwrap();
}

/// `client.counts` naming each of `open`'s newest message.
fn record_counts(api: &Path, open: &[(&str, &str)]) {
    let channels: Vec<Value> = open
        .iter()
        .map(|(id, latest)| json!({"id": id, "latest": latest, "last_read": latest}))
        .collect();
    record_call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true, "channels": channels, "mpims": [], "ims": []}),
    )
    .unwrap();
}

async fn sync(api: &Path, out: &Path, adjust: impl FnOnce(FetchOptions) -> FetchOptions) {
    let playback = tempfile::tempdir().unwrap();
    serve(api, playback.path());
    fetch_into(playback.path(), out, adjust).await.unwrap();
}

fn held(out: &Path) -> BTreeSet<(String, String)> {
    let raw = block_on_load_all(&db_path_for(out)).unwrap();
    raw.messages
        .iter()
        .map(|m| (m.channel_id.clone(), m.ts.clone()))
        .collect()
}

fn pairs(want: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    want.iter()
        .map(|(c, t)| (c.to_string(), t.to_string()))
        .collect()
}

/// C1 is quiet, C2 has a message newer than what is held, and C3 is a
/// channel `client.counts` does not list. The second sync asks only C2
/// and C3 for what is newer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_with_nothing_new_costs_no_history_call() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_listing(&api, &["C1", "C2", "C3"]);
    for c in ["C1", "C2", "C3"] {
        History::cold(c)
            .record(&api, json!([msg(T1, &format!("in {c}"))]))
            .unwrap();
        History {
            inclusive: false,
            ..History::from(c, T1)
        }
        .record(&api, json!([msg(T2, &format!("later in {c}"))]))
        .unwrap();
    }
    record_counts(&api, &[("C1", T1), ("C2", T2)]);

    sync(&api, &out, |o| o).await;
    assert_eq!(held(&out), pairs(&[("C1", T1), ("C2", T1), ("C3", T1)]));

    sync(&api, &out, |o| o).await;
    assert_eq!(
        held(&out),
        pairs(&[("C1", T1), ("C2", T1), ("C2", T2), ("C3", T1), ("C3", T2)]),
        "C1 was quiet, so its later message was not asked for"
    );
}

/// A `since` old enough that the window below reaches past it.
const SINCE: &str = "2020-01-01";
const SINCE_TS: &str = "1577836800.000000";
const WHOLE_CHANNEL_DAYS: i64 = 36500;
const TS_A: &str = "1700000000.000000";
const TS_B: &str = "1700000100.000000";
const TS_C: &str = "1700000200.000000";

/// One sync of C1, quiet at `TS_C`, at `now`: its refresh window re-reads
/// the whole channel and Slack serves `window`. Nothing above `TS_C` is
/// recorded, so a sync that asked for it would fail the channel.
async fn refresh_at(out: &Path, now: &str, window: Value) {
    let dir = tempfile::tempdir().unwrap();
    let api = dir.path().join("api");
    record_listing(&api, &["C1"]);
    History::from("C1", SINCE_TS)
        .record(
            &api,
            json!([msg(TS_A, "a"), msg(TS_B, "b"), msg(TS_C, "c")]),
        )
        .unwrap();
    History {
        latest: Some(TS_C),
        ..History::from("C1", SINCE_TS)
    }
    .record(&api, window)
    .unwrap();
    record_counts(&api, &[("C1", TS_C)]);
    sync(&api, out, |o| FetchOptions {
        since: SINCE.to_string(),
        refresh_window_days: WHOLE_CHANNEL_DAYS,
        refresh_every_minutes: 60,
        now: now.parse().unwrap(),
        ..o
    })
    .await;
}

/// The window is how a deleted message is noticed, so B going missing
/// upstream shows exactly which syncs re-read it: the one an hour after
/// the last re-read does, the one half an hour after does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_channel_rereads_its_window_once_per_interval() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let whole = json!([msg(TS_A, "a"), msg(TS_B, "b"), msg(TS_C, "c")]);
    let without_b = json!([msg(TS_A, "a"), msg(TS_C, "c")]);

    // The cold walk read everything; the next sync re-reads the window.
    refresh_at(&out, "2026-10-27T12:00:00Z", whole.clone()).await;
    refresh_at(&out, "2026-10-27T12:01:00Z", whole).await;
    assert_eq!(stored_ts(&out), [TS_A, TS_B, TS_C]);

    refresh_at(&out, "2026-10-27T12:31:00Z", without_b.clone()).await;
    assert_eq!(
        stored_ts(&out),
        [TS_A, TS_B, TS_C],
        "half an hour after a re-read, the window is not read again"
    );

    refresh_at(&out, "2026-10-27T13:01:00Z", without_b).await;
    assert_eq!(stored_ts(&out), [TS_A, TS_C], "an hour after, it is");
}
