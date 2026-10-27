//! #1374: with `dms = true`, a DM costs a history call only when
//! `client.counts` shows it open with something newer than what is held.
//! Every conversation's history is served in every run, so a DM walked
//! when it should not be lands its message and fails the assertion.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_slack::ingest::{db_path_for, FetchOptions};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History,
};
use serde_json::{json, Value};

use crate::support::{channels_with_messages, fetch_into, msg, serve};

const DM_TYPES: &str = "public_channel,private_channel,im,mpim";

/// 2025-01-01: every message below is newer than the default `since`.
const T1: &str = "1735689600.000100";
const T2: &str = "1735689600.000200";

/// A sync's recorded calls: the bridge, two 1:1 DMs and a group DM, each
/// holding one message at `T1`, and `client.counts` naming the open
/// ones' newest messages.
fn record_run(api: &Path, open: &[(&str, &str)]) {
    record_auth(api).unwrap();
    record_users(
        api,
        json!([
            {"id": "U1", "name": "picard"},
            {"id": "U2", "name": "riker"},
            {"id": "U3", "name": "data"},
        ]),
    )
    .unwrap();
    record_conversations(
        api,
        DM_TYPES,
        json!([
            {"id": "C1", "name": "bridge", "is_member": true},
            {"id": "D1", "is_im": true, "user": "U2"},
            {"id": "D2", "is_im": true, "user": "U3"},
            {"id": "G1", "is_mpim": true, "is_member": true,
             "name": "mpdm-picard--riker--data-1", "members": ["U1", "U2", "U3"]},
        ]),
    )
    .unwrap();
    for c in ["C1", "D1", "D2", "G1"] {
        History::cold(c)
            .record(api, json!([msg(T1, &format!("in {c}"))]))
            .unwrap();
        // What a run asks of a conversation walked up to `T1`.
        History {
            inclusive: false,
            ..History::from(c, T1)
        }
        .record(api, json!([msg(T2, &format!("later in {c}"))]))
        .unwrap();
    }
    let states = |kind: fn(&str) -> bool| -> Vec<Value> {
        open.iter()
            .filter(|(id, _)| kind(id))
            .map(|(id, latest)| json!({"id": id, "latest": latest, "last_read": T1}))
            .collect()
    };
    record_call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true,
               "channels": states(|id| id.starts_with('C')),
               "mpims": states(|id| id.starts_with('G')),
               "ims": states(|id| id.starts_with('D'))}),
    )
    .unwrap();
}

async fn sync(api: &Path, out: &Path, adjust: impl FnOnce(FetchOptions) -> FetchOptions) {
    let playback = tempfile::tempdir().unwrap();
    serve(api, playback.path());
    fetch_into(playback.path(), out, |o| {
        adjust(FetchOptions { dms: true, ..o })
    })
    .await
    .unwrap();
}

async fn history_scopes(out: &Path) -> BTreeSet<String> {
    let reader = datalib_pin::open_reader(&db_path_for(out)).await.unwrap();
    let scopes: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT scope FROM coverage WHERE scope LIKE 'history:%'")
            .fetch_all(&reader)
            .await
            .unwrap();
    reader.close().await;
    scopes.into_iter().collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// D1 is open with a message; G1 is open and has never held one; D2 is
/// closed. Only D1 is walked, and nothing is covered for the other two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_dms_open_with_something_new_are_walked() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1), ("D1", T1), ("G1", "0000000000.000000")]);

    sync(&api, &out, |o| o).await;

    assert_eq!(channels_with_messages(&out), set(&["C1", "D1"]));
    assert_eq!(
        history_scopes(&out).await,
        set(&["history:C1", "history:D1"]),
        "a skipped DM must not be covered: nothing read it"
    );
}

/// A DM is walked again once it has something newer than what is held,
/// and left alone while it has not; a closed DM that reopens is owed from
/// `since`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dm_is_walked_again_when_it_gets_activity() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let (run1, run2) = (dir.path().join("run1"), dir.path().join("run2"));
    record_run(&run1, &[("C1", T1), ("D1", T1), ("G1", T1)]);
    sync(&run1, &out, |o| o).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "G1"]));

    // D1 is quiet, G1 has a new message, D2 has reopened.
    record_run(&run2, &[("C1", T2), ("D1", T1), ("G1", T2), ("D2", T1)]);
    sync(&run2, &out, |o| o).await;

    let raw = datalib_etl_slack::ingest::block_on_load_all(&db_path_for(&out)).unwrap();
    let held: BTreeSet<(String, String)> = raw
        .messages
        .iter()
        .map(|m| (m.channel_id.clone(), m.ts.clone()))
        .collect();
    let want: BTreeSet<(String, String)> = [
        ("C1", T1),
        ("C1", T2),
        ("D1", T1),
        ("D2", T1),
        ("G1", T1),
        ("G1", T2),
    ]
    .iter()
    .map(|(c, t)| (c.to_string(), t.to_string()))
    .collect();
    assert_eq!(
        held, want,
        "D1 was quiet, so its later message was not asked for"
    );
}

/// Moving `since` earlier widens the window: a DM whose newest message
/// was below the old `since` is walked once it is inside the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn widening_since_walks_a_dm_it_now_reaches() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1), ("D1", T1)]);
    History::from("C1", "1735776000.000000")
        .record(&api, json!([]))
        .unwrap();

    sync(&api, &out, |o| FetchOptions {
        since: "2025-01-02".to_string(),
        ..o
    })
    .await;
    assert!(channels_with_messages(&out).is_empty());
    assert!(!history_scopes(&out).await.contains("history:D1"));

    sync(&api, &out, |o| o).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1"]));
}

/// A DM named in `dm_conversations` is walked whatever `client.counts`
/// says: the person asked for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_dm_is_walked_even_when_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1)]);

    sync(&api, &out, |o| FetchOptions {
        dm_conversations: Some(vec!["D2".to_string()]),
        ..o
    })
    .await;

    assert_eq!(channels_with_messages(&out), set(&["C1", "D2"]));
}

struct StopOnFirstTick(StopFlag);

impl ProgressSink for StopOnFirstTick {
    fn inc(&self, _delta: u64) {
        self.0.request();
    }
}

/// A run stopped part way leaves the active DMs it did not reach owed
/// and the idle ones untouched; the next run walks the rest and still
/// skips the idle one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_run_leaves_the_active_dms_owed_and_the_idle_ones_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1), ("D1", T1), ("G1", T1)]);

    let stop = StopFlag::new();
    sync(&api, &out, |o| FetchOptions {
        progress: Progress::new(Arc::new(StopOnFirstTick(stop.clone()))),
        control: DownloadControl {
            stop: stop.clone(),
            ..Default::default()
        },
        ..o
    })
    .await;
    assert!(stop.requested());
    assert_eq!(history_scopes(&out).await.len(), 1);

    sync(&api, &out, |o| o).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "G1"]));
    assert!(!history_scopes(&out).await.contains("history:D2"));
}
