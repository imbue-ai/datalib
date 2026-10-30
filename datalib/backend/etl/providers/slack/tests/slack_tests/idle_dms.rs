//! #1374: with `dms = true`, an open DM costs a history call only when
//! `client.counts` shows it with something newer than what is held. A DM
//! closed in the sidebar is one `client.counts` leaves out, so it is
//! walked once and then checked once a day. Every conversation's history
//! is served in every run, so a DM walked when it should not be lands its
//! message and fails the assertion.

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

use crate::support::{channels_with_messages, fetch_into, msg, serve, stored_ts};

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
/// closed and has never been walked. D1 is walked for its message and D2
/// because nothing says what it holds; G1 is not, and nothing is covered
/// for it.
///
/// Before, a closed DM was never walked: a first sync lost every message
/// in a DM the account had closed, and nothing said so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_dm_is_walked_on_the_first_sync() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1), ("D1", T1), ("G1", "0000000000.000000")]);

    sync(&api, &out, |o| o).await;

    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "D2"]));
    assert_eq!(
        history_scopes(&out).await,
        set(&["history:C1", "history:D1", "history:D2"]),
        "a skipped DM must not be covered: nothing read it"
    );
}

/// The run's clock, `hours` after the first sync's.
fn at(hours: i64) -> chrono::DateTime<chrono::Utc> {
    "2026-10-27T12:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap()
        + chrono::Duration::hours(hours)
}

/// A closed DM walked once is checked again a day later, not on every
/// run: Slack reopens a DM when a message arrives, but one closed again
/// before the next run shows nothing in `client.counts`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_dm_is_checked_again_a_day_after_its_last_walk() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1), ("D1", T1), ("G1", T1)]);

    sync(&api, &out, |o| FetchOptions { now: at(0), ..o }).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "D2", "G1"]));

    sync(&api, &out, |o| FetchOptions { now: at(6), ..o }).await;
    assert_eq!(
        stored_ts(&out).iter().filter(|t| *t == T2).count(),
        0,
        "six hours on, the closed DM is not walked again"
    );

    sync(&api, &out, |o| FetchOptions { now: at(25), ..o }).await;
    let raw = datalib_etl_slack::ingest::block_on_load_all(&db_path_for(&out)).unwrap();
    let later: BTreeSet<&str> = raw
        .messages
        .iter()
        .filter(|m| m.ts == T2)
        .map(|m| m.channel_id.as_str())
        .collect();
    assert_eq!(
        later,
        BTreeSet::from(["D2"]),
        "a day on, the closed DM is checked and the open quiet ones are not"
    );
}

/// The coverage rows of `scope`, as `(lo, hi)`.
async fn spans(out: &Path, scope: &str) -> Vec<(String, String)> {
    let reader = datalib_pin::open_reader(&db_path_for(out)).await.unwrap();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT lo, hi FROM coverage WHERE scope = ? ORDER BY lo")
            .bind(scope)
            .fetch_all(&reader)
            .await
            .unwrap();
    reader.close().await;
    rows
}

/// A DM walked and found empty is covered where its walk began, so a
/// reader can tell "read, nothing there" from "never read", and the next
/// run does not walk it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_closed_dm_is_covered_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let (run1, run2) = (dir.path().join("run1"), dir.path().join("run2"));
    record_run(&run1, &[("C1", T1), ("D1", T1), ("G1", T1)]);
    History::cold("D3").record(&run1, json!([])).unwrap();
    record_conversations(
        &run1,
        DM_TYPES,
        json!([
            {"id": "C1", "name": "bridge", "is_member": true},
            {"id": "D1", "is_im": true, "user": "U2"},
            {"id": "D2", "is_im": true, "user": "U3"},
            {"id": "D3", "is_im": true, "user": "U3"},
            {"id": "G1", "is_mpim": true, "is_member": true,
             "name": "mpdm-picard--riker--data-1", "members": ["U1", "U2", "U3"]},
        ]),
    )
    .unwrap();
    sync(&run1, &out, |o| FetchOptions { now: at(0), ..o }).await;
    let since = spans(&out, "history:D3").await;
    assert_eq!(since.len(), 1, "the empty DM is covered: {since:?}");
    assert_eq!(
        since[0].0, since[0].1,
        "from where the walk began, to there"
    );

    // The same listing, and now D3 has a message to give.
    std::fs::rename(&run1, &run2).unwrap();
    History::cold("D3")
        .record(&run2, json!([msg(T1, "in D3")]))
        .unwrap();
    sync(&run2, &out, |o| FetchOptions { now: at(1), ..o }).await;
    assert!(
        !channels_with_messages(&out).contains("D3"),
        "an hour on, the empty closed DM is not walked again"
    );
}

/// A DM is walked again once it has something newer than what is held,
/// and left alone while it has not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dm_is_walked_again_when_it_gets_activity() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let (run1, run2) = (dir.path().join("run1"), dir.path().join("run2"));
    record_run(&run1, &[("C1", T1), ("D1", T1), ("G1", T1)]);
    sync(&run1, &out, |o| o).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "D2", "G1"]));

    // D1 is quiet, G1 has a new message, D2 has reopened with nothing new.
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
    // D2 and G1 are closed: walked from the narrow `since` first, then
    // below it.
    for c in ["D2", "G1"] {
        History::from(c, "1735776000.000000")
            .record(&api, json!([]))
            .unwrap();
        History {
            latest: Some("1735776000.000000"),
            ..History::cold(c)
        }
        .record(&api, json!([msg(T1, &format!("in {c}"))]))
        .unwrap();
    }

    sync(&api, &out, |o| FetchOptions {
        since: "2025-01-02".to_string(),
        ..o
    })
    .await;
    assert!(channels_with_messages(&out).is_empty());
    assert!(!history_scopes(&out).await.contains("history:D1"));

    sync(&api, &out, |o| o).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "D2", "G1"]));
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

/// A DM named in `dm_conversations` is held to `client.counts` like any
/// other once it has been walked. Before, every named DM was walked on
/// every run, so naming two hundred of them made each refresh take
/// minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_dm_with_nothing_new_is_not_walked_again() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_run(&api, &[("C1", T1), ("D1", T1)]);
    let named = |o: FetchOptions| FetchOptions {
        dm_conversations: Some(vec!["D1".to_string()]),
        ..o
    };

    sync(&api, &out, named).await;
    sync(&api, &out, named).await;

    assert_eq!(
        stored_ts(&out),
        [T1, T1],
        "D1 showed nothing newer than T1, so its later message was not asked for"
    );
}

struct StopOnFirstTick(StopFlag);

impl ProgressSink for StopOnFirstTick {
    fn inc(&self, _delta: u64) {
        self.0.request();
    }
}

/// A run stopped part way leaves the DMs it did not reach owed; the
/// next run walks the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_run_leaves_the_dms_it_did_not_reach_owed() {
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
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "D2", "G1"]));
}
