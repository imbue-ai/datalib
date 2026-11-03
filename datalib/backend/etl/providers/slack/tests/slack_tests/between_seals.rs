//! The walk writes everything between two seals in one transaction:
//! every SQL commit of a doltlite file costs a fresh copy of each page
//! it touched, so a commit per page of history or per thread grew a
//! store several times over (INGEST.md § "One transaction between two
//! seals").

use std::path::Path;

use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_slack::ingest::{db_path_for, fetch, FetchOptions, RawDb, MESSAGES_PER_PAGE};
use datalib_etl_slack::recorded::{record_call, record_workspace};
use datalib_etl_web::interrupt::{run, How};
use datalib_etl_web::playback;
use serde_json::json;

use crate::every_cut::ts;
use crate::support::{one_at_a_time, stored_ts, Tree};

/// A thread in `#bridge`, and one message in each of two more channels.
fn record(api: &Path) {
    record_workspace(api, &["C1_BRIDGE", "C2_ENG", "C3_TEN"], |i, _| {
        let n = 10 * i as u64;
        if i > 0 {
            return json!([{"ts": ts(n), "user": "U1", "text": "Report."}]);
        }
        json!([{"ts": ts(n), "user": "U1", "text": "Status?", "thread_ts": ts(n),
                "reply_count": 1, "latest_reply": ts(n + 1)}])
    })
    .unwrap();
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1_BRIDGE", "ts": ts(0), "limit": MESSAGES_PER_PAGE}),
        json!({"ok": true, "has_more": false, "messages": [
            {"ts": ts(0), "user": "U1", "text": "Status?", "thread_ts": ts(0),
             "reply_count": 1, "latest_reply": ts(1)},
            {"ts": ts(1), "user": "U1", "text": "All decks ready.", "thread_ts": ts(0)},
        ]}),
    )
    .unwrap();
}

/// One download with no sealer, one request at a time, killed at its
/// `at`-th request; then everything SQL committed is kept, as the
/// interruption tests keep it. Returns the requests it made.
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
    let ran = run(
        at,
        How::Kill,
        stop.clone(),
        one_at_a_time(stop, fetch(opts)),
    )
    .await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    ran.requests
}

/// Each page of history and each thread was a transaction of its own,
/// so a run killed at its last request had committed every message
/// before it. With no seal due, nothing the walk wrote is committed
/// until the run ends; the next run does it all again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_walk_commits_nothing_until_a_seal_or_the_end() {
    let tape = Tree::new();
    record(&tape.api);
    tape.serve();
    let scratch = tempfile::tempdir().unwrap();
    let (whole, cut) = (scratch.path().join("whole"), scratch.path().join("cut"));
    playback::scope(&tape.playback, async {
        let requests = download(&whole, None).await;
        assert_eq!(stored_ts(&whole).len(), 4, "three messages and a reply");

        download(&cut, Some(requests)).await;
        assert_eq!(
            stored_ts(&cut),
            Vec::<String>::new(),
            "the walk's pages and thread before the last request were committed"
        );

        download(&cut, None).await;
        assert_eq!(stored_ts(&cut), stored_ts(&whole));
    })
    .await;
}
