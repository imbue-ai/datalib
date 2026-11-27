//! A call Slack answers with HTTP 200 and an error it calls transient is
//! asked again in the same run, and what it was for lands in that run.

use std::path::Path;
use std::time::{Duration, Instant};

use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_slack::ingest::{db_path_for, fetch, FetchOptions, RawDb, MESSAGES_PER_PAGE};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use datalib_etl_web::playback::{self, Asked, Playback};
use datalib_etl_web::retry::{self, RetryGuard};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::{serve, stored_ts};
use datalib_sql::test_sql;

const A: &str = "1735689600.000100";
const REPLY: &str = "1735689600.000150";
const B: &str = "1735689600.000200";

fn root() -> Value {
    json!({"ts": A, "user": "U1", "text": "status report", "thread_ts": A,
           "reply_count": 1, "latest_reply": REPLY})
}

/// `#bridge` holding a thread rooted at `A`, whose replies are answered
/// by `replies`.
fn record_world(api: &Path, replies: Value) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true, "is_archived": false}]),
    )
    .unwrap();
    record_call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true, "channels": [], "mpims": [], "ims": []}),
    )
    .unwrap();
    for filter in ["saved", "completed", "archived"] {
        record_call(
            api,
            "saved.list",
            json!({"filter": filter, "limit": "50"}),
            json!({"ok": true, "saved_items": []}),
        )
        .unwrap();
    }
    History::cold("C1")
        .record(
            api,
            json!([{"ts": B, "user": "U1", "text": "second"}, root()]),
        )
        .unwrap();
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1", "ts": A, "limit": MESSAGES_PER_PAGE}),
        replies,
    )
    .unwrap();
}

async fn problems(out: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows = test_sql!("SELECT scope_key, severity FROM problems ORDER BY scope_key")
        .query_as()
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    rows
}

/// Waits until `asked` has sent a request to a URL containing `part`.
async fn until_asked(asked: &Asked, part: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while asked.count(part) == 0 {
        assert!(Instant::now() < deadline, "nothing asked for {part}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `internal_error` was taken as a permanent failure: the thread was a
/// problem row and its replies were missing until the next sync. The
/// tape answers `internal_error` until the first ask is seen, then the
/// replies; the retry may come before or after that swap, and either way
/// the run ends holding them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_slack_failed_with_internal_error_lands_in_the_same_run() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let tape = d.path().join("playback");

    let failing = d.path().join("api_failing");
    record_world(&failing, json!({"ok": false, "error": "internal_error"}));
    serve(&failing, &tape);
    let answering = d.path().join("api_answering");
    record_world(
        &answering,
        json!({"ok": true, "has_more": false, "messages": [
            root(),
            {"ts": REPLY, "user": "U1", "text": "all nominal", "thread_ts": A},
        ]}),
    );

    let asked = Asked::default();
    let db = RawDb::open(&db_path_for(&out)).await.unwrap();
    let backoff = Duration::from_millis(20);
    let guard = RetryGuard::new(
        Duration::from_secs(60),
        1000,
        backoff,
        backoff,
        StopFlag::new(),
    );
    let download = retry::scope(
        guard,
        fetch(FetchOptions {
            refresh_window_days: 0,
            members_only: false,
            media: false,
            ..FetchOptions::new(db.clone())
        }),
    );
    let swap = async {
        until_asked(&asked, "conversations.replies").await;
        serve(&answering, &tape);
    };
    let (ran, ()) = tokio::join!(
        playback::scope(Playback::at(&tape).asked(asked.clone()), download),
        swap
    );
    ran.unwrap();
    db.commit_all("test").await.unwrap();
    db.close().await;

    assert!(
        asked.count("conversations.replies") >= 2,
        "the replies were asked for again"
    );
    assert_eq!(stored_ts(&out), [A, REPLY, B]);
    assert_eq!(problems(&out).await, [], "nothing is left owed");
}
