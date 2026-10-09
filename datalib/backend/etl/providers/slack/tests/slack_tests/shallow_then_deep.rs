//! The two things that decide how soon a first sync is worth reading:
//! `replies = false` mirrors top-level messages and leaves every thread
//! owed, for a later run to fetch without walking a channel again
//! (`replies_channels` does that for all but the channels it names); a
//! run walks every channel's history before any channel's threads; and
//! `archived` brings in the channels a default listing leaves out.

use std::path::Path;
use std::sync::{Arc, Mutex};

use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_users, record_workspace, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};

use crate::support::{channels_with_messages, fetch_into, msg, stored_ts, Tree};

const CHANNELS: [&str; 2] = ["C1", "C2"];

/// Channel `i`'s thread: a root on 2025-01-01, after the default `since`, and its one reply.
fn root_ts(i: usize) -> String {
    format!("17356896{i}0.000000")
}

fn reply_ts(i: usize) -> String {
    format!("17356896{i}5.000000")
}

fn root(i: usize) -> Value {
    json!({"ts": root_ts(i), "user": "U1", "text": "status report", "thread_ts": root_ts(i),
           "reply_count": 1, "latest_reply": reply_ts(i)})
}

/// Two channels, each a cold start whose one message is a thread root,
/// with the thread's replies and the "anything newer?" page a second run
/// asks each channel for.
fn record_two_threads(api: &Path) {
    record_workspace(api, &CHANNELS, |i, _| json!([root(i)])).unwrap();
    for (i, channel) in CHANNELS.iter().enumerate() {
        record_call(
            api,
            "conversations.replies",
            json!({"channel": channel, "ts": root_ts(i), "limit": "200"}),
            json!({"ok": true, "has_more": false, "messages": [
                root(i),
                {"ts": reply_ts(i), "user": "U1", "text": "all nominal", "thread_ts": root_ts(i)},
            ]}),
        )
        .unwrap();
        History {
            inclusive: false,
            ..History::from(channel, &root_ts(i))
        }
        .record(api, json!([]))
        .unwrap();
    }
}

/// The channels and threads a run could not read. The account's own
/// state is not on this tape, and its two listings fail every run.
async fn unread(out: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let keys = sqlx::query_scalar(
        "SELECT scope_key FROM problems \
         WHERE scope_key LIKE 'listing:conversations.%' OR scope_key LIKE 'threads:%' \
         ORDER BY scope_key",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    keys
}

/// A shallow run stores the roots and says how many threads it left; the
/// deep run after it fetches exactly those, from the stored roots.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_off_leaves_threads_owed_and_a_later_run_fetches_them() {
    let t = Tree::new();
    record_two_threads(&t.api);
    t.serve();

    let shallow = fetch_into(&t.out, |o| FetchOptions {
        replies: false,
        ..o
    })
    .await
    .unwrap();
    assert_eq!(shallow.messages, 2);
    assert_eq!(shallow.replies, 0, "no thread was asked for");
    assert_eq!(shallow.threads_owed, 2, "and the run says so");
    assert_eq!(stored_ts(&t.out), vec![root_ts(0), root_ts(1)]);

    let deep = fetch_into(&t.out, |o| o).await.unwrap();
    assert_eq!(deep.messages, 0, "nothing new at the top level");
    assert_eq!(deep.replies, 2, "both owed threads were fetched");
    assert_eq!(deep.threads_owed, 0);
    assert_eq!(
        stored_ts(&t.out),
        vec![root_ts(0), reply_ts(0), root_ts(1), reply_ts(1)]
    );
    // Every request either run made was on the tape: a history walk
    // repeated from `since` would have missed it and left a problem row.
    assert_eq!(unread(&t.out).await, Vec::<String>::new());
}

/// `replies_channels` reads the named channel's threads and leaves the
/// rest owed, counted like the threads of a run with replies off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_channels_reads_only_the_named_channels_threads() {
    let t = Tree::new();
    record_two_threads(&t.api);
    t.serve();

    let summary = fetch_into(&t.out, |o| FetchOptions {
        replies_channels: Some(vec!["c2".into()]),
        ..o
    })
    .await
    .unwrap();
    assert_eq!((summary.replies, summary.threads_owed), (1, 1));
    assert_eq!(
        stored_ts(&t.out),
        vec![root_ts(0), root_ts(1), reply_ts(1)],
        "C2's reply and not C1's"
    );
}

/// `archived` asks Slack for archived channels as well, and walks them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_mirrors_an_archived_channel() {
    let t = Tree::new();
    record_auth(&t.api).unwrap();
    record_users(&t.api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_call(
        &t.api,
        "conversations.list",
        json!({"exclude_archived": "false", "limit": "200", "types": CHANNEL_TYPES}),
        json!({"ok": true, "has_more": false, "channels": [
            {"id": "C1", "name": "c1", "is_member": true, "is_archived": false},
            {"id": "C9", "name": "old-project", "is_member": false, "is_archived": true},
        ]}),
    )
    .unwrap();
    for (i, channel) in ["C1", "C9"].iter().enumerate() {
        History::cold(channel)
            .record(&t.api, json!([msg(&root_ts(i), "hello")]))
            .unwrap();
    }
    t.serve();

    let summary = fetch_into(&t.out, |o| FetchOptions {
        archived: true,
        ..o
    })
    .await
    .unwrap();
    assert_eq!(summary.messages, 2);
    assert_eq!(
        channels_with_messages(&t.out),
        ["C1", "C9"].map(String::from).into_iter().collect()
    );
}

/// What the run said it was doing, in order.
#[derive(Default, Clone)]
struct Said(Arc<Mutex<Vec<String>>>);

impl ProgressSink for Said {
    fn set_message(&self, msg: &str) {
        self.0.lock().unwrap().push(msg.to_string());
    }
}

/// Each channel's history is walked before the first thread is read, so a
/// run cut short has the whole workspace's top level rather than every
/// thread of its first channels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_channels_history_is_walked_before_any_thread() {
    let t = Tree::new();
    record_two_threads(&t.api);
    t.serve();

    let said = Said::default();
    let summary = fetch_into(&t.out, |o| FetchOptions {
        progress: Progress::new(Arc::new(said.clone())),
        ..o
    })
    .await
    .unwrap();
    assert_eq!((summary.messages, summary.replies), (2, 2));

    let said = said.0.lock().unwrap().clone();
    let last_listing = said
        .iter()
        .rposition(|m| m.ends_with(": listing"))
        .expect("each channel's walk names itself");
    let first_thread = said
        .iter()
        .position(|m| m.contains("replies="))
        .expect("each thread read reports its replies");
    assert!(
        last_listing < first_thread,
        "a thread was read before the last channel's history was walked: {said:?}"
    );
}
