//! A run fetches newest first across the whole workspace, so a run cut
//! off part way has stored the newest messages, not the whole of
//! whichever channels were listed first.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_slack::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, CHANNEL_TYPES, DEFAULT_SINCE_TS,
};
use datalib_etl_web::interrupt::{run, How};
use datalib_etl_web::playback;
use serde_json::{json, Value};

use crate::every_cut::ts;
use crate::support::{stored_ts, Tree};

/// `#archive`'s thread: its root is the channel's third-oldest message,
/// and its replies are newer than anything in `#bridge`.
const ROOT: u64 = 2;
const REPLIES: [u64; 2] = [104, 105];

fn said(n: u64) -> Value {
    json!({"ts": ts(n), "user": "U1", "text": format!("message {n}")})
}

/// Two pages of `channel`'s history from the default `since`, newest
/// first.
fn record_two_pages(api: &Path, channel: &str, first: Vec<Value>, second: Vec<Value>) {
    let page = json!({"channel": channel, "include_all_metadata": "true", "inclusive": "true",
                      "limit": "200", "oldest": DEFAULT_SINCE_TS});
    record_call(
        api,
        "conversations.history",
        page.clone(),
        json!({"ok": true, "messages": first, "has_more": true,
               "response_metadata": {"next_cursor": "page2"}}),
    )
    .unwrap();
    let mut next = page;
    next["cursor"] = json!("page2");
    record_call(
        api,
        "conversations.history",
        next,
        json!({"ok": true, "messages": second, "has_more": false}),
    )
    .unwrap();
}

/// `#archive` (listed first) holds only old messages, `#bridge` only
/// new ones.
fn record_workspace(api: &Path) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([
            {"id": "C1_ARCHIVE", "name": "archive", "is_member": true},
            {"id": "C2_BRIDGE", "name": "bridge", "is_member": true},
        ]),
    )
    .unwrap();
    let root = json!({"ts": ts(ROOT), "user": "U1", "text": "Old question.",
                      "thread_ts": ts(ROOT), "reply_count": REPLIES.len(),
                      "latest_reply": ts(REPLIES[1])});
    record_two_pages(
        api,
        "C1_ARCHIVE",
        vec![said(3), root.clone()],
        vec![said(1), said(0)],
    );
    record_two_pages(
        api,
        "C2_BRIDGE",
        vec![said(103), said(102)],
        vec![said(101), said(100)],
    );
    let mut messages = vec![root];
    messages.extend(REPLIES.iter().map(|&n| {
        json!({"ts": ts(n), "user": "U1", "text": format!("reply {n}"), "thread_ts": ts(ROOT)})
    }));
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1_ARCHIVE", "ts": ts(ROOT), "limit": "200"}),
        json!({"ok": true, "has_more": false, "messages": messages}),
    )
    .unwrap();
}

/// One download into `dir`, killed at its `at`-th request. Returns the
/// requests it made.
async fn download(dir: &Path, at: Option<u64>) -> u64 {
    let db = RawDb::open(&db_path_for(dir)).await.unwrap();
    let stop = StopFlag::new();
    let opts = FetchOptions {
        refresh_window_days: 0,
        members_only: false,
        media: false,
        control: DownloadControl {
            stop: stop.clone(),
            ..Default::default()
        },
        ..FetchOptions::new(db.clone())
    };
    let ran = run(at, How::Kill, stop, fetch(opts)).await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    ran.requests
}

/// The messages in the order they landed: what each request added to the
/// store, read off runs killed one request later each time.
async fn landing_order() -> Vec<Vec<String>> {
    let scratch = tempfile::tempdir().unwrap();
    let n = download(&scratch.path().join("whole"), None).await;
    let mut order = Vec::new();
    let mut before: BTreeSet<String> = BTreeSet::new();
    for k in 1..=n + 1 {
        let dir = scratch.path().join(format!("cut-{k}"));
        download(&dir, Some(k)).await;
        let stored: BTreeSet<String> = stored_ts(&dir).into_iter().collect();
        let landed: Vec<String> = stored.difference(&before).cloned().collect();
        if !landed.is_empty() {
            order.push(landed);
        }
        before = stored;
    }
    order
}

/// Before this order, `#archive` was walked to its bottom and its thread
/// read before `#bridge` was asked anything, so a run cut off early held
/// last year and none of this week.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_stores_the_newest_messages_of_the_workspace_first() {
    let tape = Tree::new();
    record_workspace(&tape.api);
    tape.serve();
    let order = playback::scope(&tape.playback, landing_order()).await;
    let pair = |a: u64, b: u64| vec![ts(a), ts(b)];
    assert_eq!(
        order,
        vec![
            // Every channel's first page, in the order they were listed.
            pair(ROOT, 3),
            pair(102, 103),
            // The thread's replies are newer than the rest of `#bridge`.
            pair(REPLIES[0], REPLIES[1]),
            pair(100, 101),
            pair(0, 1),
        ]
    );
}
