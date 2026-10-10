//! A reply to a thread whose root history will not read again is found
//! by `search.messages`, and the root it names is read again so the
//! thread is owed.

use std::path::Path;

use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{record_call, History};
use serde_json::{json, Value};

use crate::support::{fetch_into, msg, record_general, stored_ts, Tree};

/// A thread from long before either run, and the channel's newest
/// top-level message.
const ROOT: &str = "1710000000.000100";
const FIRST_REPLY: &str = "1710000100.000100";
const NEWEST: &str = "1780000000.000100";
/// Posted between the two runs.
const LATE_REPLY: &str = "1790100000.000100";

/// 1790000000, then three days later: the search of the second run
/// spans `[first - 30min, second - 30min]`.
const FIRST_RUN: &str = "2026-09-21T14:13:20Z";
const SECOND_RUN: &str = "2026-09-24T14:13:20Z";
const SECOND_RUN_QUERY: &str = "is:thread in:<#C1> after:2026-09-19 before:2026-09-26";

fn root(replies: u32, latest_reply: &str) -> Value {
    json!({"ts": ROOT, "user": "U1", "text": "away team report", "thread_ts": ROOT,
           "reply_count": replies, "latest_reply": latest_reply})
}

fn reply(ts: &str, text: &str) -> Value {
    json!({"ts": ts, "user": "U1", "text": text, "thread_ts": ROOT})
}

fn record_replies(api: &Path, messages: Value) {
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1", "ts": ROOT, "limit": "200"}),
        json!({"ok": true, "has_more": false, "messages": messages}),
    )
    .unwrap();
}

fn search_match(ts: &str, thread_ts: &str) -> Value {
    let p = ts.replace('.', "");
    json!({"ts": ts, "channel": {"id": "C1", "name": "general"}, "text": "",
           "permalink": format!("https://ncc-1701.slack.com/archives/C1/p{p}?thread_ts={thread_ts}&cid=C1")})
}

async fn run(out: &Path, now: &str) {
    fetch_into(out, |o| FetchOptions {
        search_replies: true,
        now: now.parse().unwrap(),
        ..o
    })
    .await
    .unwrap();
}

async fn search_problems(out: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT scope_key FROM problems WHERE scope_key LIKE '%search.messages%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    rows
}

/// The headline: the root is years older than any refresh window, and
/// the run after the reply stores it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_reply_to_an_old_thread_is_found_by_search() {
    // The first run walks the channel cold, so every root it has is read
    // fresh: its reply time is settled without a search, which the tape
    // could not answer.
    let first = Tree::new();
    record_general(&first.api);
    History::cold("C1")
        .record(
            &first.api,
            json!([msg(NEWEST, "newest"), root(1, FIRST_REPLY)]),
        )
        .unwrap();
    record_replies(
        &first.api,
        json!([root(1, FIRST_REPLY), reply(FIRST_REPLY, "all decks report")]),
    );
    first.serve();
    run(&first.out, FIRST_RUN).await;
    assert_eq!(stored_ts(&first.out), [ROOT, FIRST_REPLY, NEWEST]);
    assert_eq!(search_problems(&first.out).await, [] as [&str; 0]);

    // The second run: history has nothing newer, and lists no root again.
    let second = Tree::new();
    record_general(&second.api);
    History {
        inclusive: false,
        ..History::from("C1", NEWEST)
    }
    .record(&second.api, json!([]))
    .unwrap();
    record_call(
        &second.api,
        "search.messages",
        json!({"query": SECOND_RUN_QUERY, "count": "100", "sort": "timestamp",
               "sort_dir": "desc", "page": "1"}),
        json!({"ok": true, "messages": {
            "total": 2,
            "paging": {"count": 100, "total": 2, "page": 1, "pages": 1},
            "matches": [search_match(LATE_REPLY, ROOT), search_match(ROOT, ROOT)],
        }}),
    )
    .unwrap();
    record_call(
        &second.api,
        "conversations.history",
        json!({"channel": "C1", "oldest": ROOT, "latest": ROOT, "inclusive": "true",
               "include_all_metadata": "true", "limit": "1"}),
        json!({"ok": true, "messages": [root(2, LATE_REPLY)], "has_more": false}),
    )
    .unwrap();
    record_replies(
        &second.api,
        json!([
            root(2, LATE_REPLY),
            reply(FIRST_REPLY, "all decks report"),
            reply(LATE_REPLY, "one more thing, Captain"),
        ]),
    );
    second.serve();
    run(&first.out, SECOND_RUN).await;
    assert_eq!(
        stored_ts(&first.out),
        [ROOT, FIRST_REPLY, NEWEST, LATE_REPLY],
        "the search found the reply, and the root it named was read again"
    );
    assert_eq!(search_problems(&first.out).await, [] as [&str; 0]);

    // The same now again: the reply time up to it is searched, so the run
    // asks nothing the tape lacks.
    run(&first.out, SECOND_RUN).await;
    assert_eq!(search_problems(&first.out).await, [] as [&str; 0]);
}
