//! A channel's history is walked newest first, a page at a time, and
//! what a run still owes of it is the range wanted minus the stretches
//! its pages covered.

use datalib_etl_slack::ingest::{db_path_for, RawDb};
use datalib_etl_slack::recorded::{record_call, History, DEFAULT_SINCE_TS};
use serde_json::json;

use crate::support::{fetch_into, msg, record_general, stored_ts, Tree};

const A: &str = "1735689600.000100";
const B: &str = "1735689600.000200";
const C: &str = "1735689600.000300";

/// A walk that stored its first page and then failed had stored the
/// newest messages. The next run used to resume above the newest stored
/// one, and what was under the first page was never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_that_fails_after_its_first_page_is_finished_from_under_it() {
    let first = Tree::new();
    record_general(&first.api);
    let cold = json!({"channel": "C1", "include_all_metadata": "true", "inclusive": "true",
                      "limit": "200", "oldest": DEFAULT_SINCE_TS});
    record_call(
        &first.api,
        "conversations.history",
        cold.clone(),
        json!({"ok": true, "messages": [msg(C, "c"), msg(B, "b")], "has_more": true,
               "response_metadata": {"next_cursor": "page2"}}),
    )
    .unwrap();
    let mut second_page = cold;
    second_page["cursor"] = json!("page2");
    record_call(
        &first.api,
        "conversations.history",
        second_page,
        json!({"ok": false, "error": "internal_error"}),
    )
    .unwrap();
    first.serve();
    fetch_into(&first.playback, &first.out, |o| o)
        .await
        .expect("a channel that fails is a problem row, not a failed run");
    assert_eq!(stored_ts(&first.out), [B, C]);

    // The next run asks for what is newer than `C`, and for what is
    // under the page it did store.
    let second = Tree::new();
    record_general(&second.api);
    History {
        inclusive: false,
        ..History::from("C1", C)
    }
    .record(&second.api, json!([]))
    .unwrap();
    History {
        latest: Some(B),
        ..History::cold("C1")
    }
    .record(&second.api, json!([msg(B, "b"), msg(A, "a")]))
    .unwrap();
    second.serve();
    fetch_into(&second.playback, &first.out, |o| o)
        .await
        .unwrap();
    assert_eq!(stored_ts(&first.out), [A, B, C]);

    let db = RawDb::open(&db_path_for(&first.out)).await.unwrap();
    let problems: Vec<String> = sqlx::query_scalar(
        "SELECT scope_key FROM problems WHERE scope_key LIKE 'listing:conversations.history%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    assert_eq!(problems, [] as [&str; 0], "the channel walked this time");
}
