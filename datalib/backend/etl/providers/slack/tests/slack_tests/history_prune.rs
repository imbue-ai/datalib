//! Slack has no tombstones and no changes cursor: a deleted message just
//! stops appearing in `conversations.history`. The only way to see that is
//! to re-walk a bounded range and compare, which the trailing
//! `refresh_window_days` pass already does. These are two-run playback
//! tests over that: run 1 mirrors a channel, run 2 serves a refresh window
//! that omits one message, and the question is what happens to our copy.

use std::path::Path;

use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{record_call, History};
use serde_json::{json, Value};

use crate::support::{fetch_into, msg, record_general, stored_ts, Tree};

/// Far enough back that `refresh_window_days: WHOLE_CHANNEL_DAYS` covers
/// every message here, so the window pass re-walks the whole channel.
///
/// Deliberately in the 10-digit-epoch era. Slack timestamps are compared
/// as *strings* throughout this provider — in the window bounds and in the
/// `ts BETWEEN` the prune issues — so a `since` before 2001-09-09 sorts
/// above every real message ("946..." > "17...") and the window silently
/// matches nothing. Every genuine Slack ts is 10 digits, so this only ever
/// bites a fixture; it cost an afternoon here, hence the note.
const SINCE: &str = "2020-01-01";
const TS_A: &str = "1700000000.000000";
const TS_B: &str = "1700000100.000000";
const TS_C: &str = "1700000200.000000";

/// `datetime_to_slack_ts` of UTC midnight on `SINCE` — the `oldest` the
/// downloader sends on a cold start, and (because the refresh window
/// reaches further back than it) the window pass's `oldest` too.
const SINCE_TS: &str = "1577836800.000000";

/// The window is counted back from the wall clock, so it is set a century
/// wide: no calendar date puts its start after `SINCE`, which is what keeps
/// the window pass's `oldest` at `SINCE_TS` and the recorded call matching.
const WHOLE_CHANNEL_DAYS: i64 = 36500;

/// Two replies on the thread rooted at `TS_A`, both between the channel's
/// oldest and newest top-level messages.
const TS_REPLY_1: &str = "1700000050.000000";
const TS_REPLY_2: &str = "1700000150.000000";

fn thread_root() -> Value {
    json!({"ts": TS_A, "user": "U1", "text": "status report", "thread_ts": TS_A,
           "reply_count": 2, "latest_reply": TS_REPLY_2})
}

/// The cold start of a channel whose first message is a two-reply thread:
/// history lists the root and not the replies, which only
/// `conversations.replies` returns.
fn write_cold_start_with_thread(api: &Path) {
    History::from("C1", SINCE_TS)
        .record(api, json!([thread_root(), msg(TS_B, "b"), msg(TS_C, "c")]))
        .unwrap();
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1", "ts": TS_A, "limit": "200"}),
        json!({"ok": true, "has_more": false, "messages": [
            thread_root(),
            {"ts": TS_REPLY_1, "user": "U1", "text": "shields holding", "thread_ts": TS_A},
            {"ts": TS_REPLY_2, "user": "U1", "text": "all nominal", "thread_ts": TS_A},
        ]}),
    )
    .unwrap();
}

/// What `replies_pages` holds as the newest reply of the `TS_A` thread.
async fn recorded_latest_reply(out: &Path) -> Option<String> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let held: Option<Option<String>> =
        sqlx::query_scalar("SELECT latest_reply FROM replies_pages WHERE thread_ts = ?")
            .bind(TS_A)
            .fetch_optional(db.pool())
            .await
            .unwrap();
    db.close().await;
    held.flatten()
}

/// The cold start: all three messages from `SINCE`.
fn write_cold_start(api: &Path) {
    History::from("C1", SINCE_TS)
        .record(api, json!([msg(TS_A, "a"), msg(TS_B, "b"), msg(TS_C, "c")]))
        .unwrap();
}

/// The walk of what is newer than `C`, which finds nothing.
fn write_nothing_new(api: &Path) {
    History {
        inclusive: false,
        ..History::from("C1", TS_C)
    }
    .record(api, json!([]))
    .unwrap();
}

/// The refresh window's re-walk of `[SINCE, C]`.
fn write_window(api: &Path, messages: Value, has_more: bool) {
    History {
        latest: Some(TS_C),
        has_more,
        ..History::from("C1", SINCE_TS)
    }
    .record(api, messages)
    .unwrap();
}

async fn run_fetch(out: &Path, refresh_window_days: i64) -> usize {
    fetch_into(out, |o| FetchOptions {
        since: SINCE.into(),
        refresh_window_days,
        ..o
    })
    .await
    .unwrap()
    .pruned
}

/// The headline: a message that vanishes from a re-walked window is
/// deleted from our copy too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_missing_from_a_rewalked_window_is_deleted() {
    let t = Tree::new();
    record_general(&t.api);

    // Run 1: cold start, three messages.
    write_cold_start(&t.api);
    // Run 2: the walk of what is newer finds nothing, then
    // the refresh window re-walks `[since, C]` — and B is gone from it.
    write_nothing_new(&t.api);
    write_window(&t.api, json!([msg(TS_A, "a"), msg(TS_C, "c")]), false);

    t.serve();

    run_fetch(&t.out, 0).await;
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_A.to_string(), TS_B.to_string(), TS_C.to_string()],
        "run 1 mirrors all three",
    );

    let pruned = run_fetch(&t.out, WHOLE_CHANNEL_DAYS).await;
    assert_eq!(pruned, 1, "the run must report the deletion it acted on");
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_A.to_string(), TS_C.to_string()],
        "B is gone from a range Slack re-served in full, so it was deleted",
    );
}

/// The other half, and the one that makes the feature safe to ship: with
/// no refresh window there is no re-walk, so nothing was re-enumerated and
/// nothing may be deleted.
///
/// Without this, "prune what the walk did not return" would delete every
/// stored message on every run: the walk of what is newer returns nothing
/// older, which is indistinguishable from "everything older was deleted"
/// to any check that does not know the walk's bounds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_refresh_window_means_no_prune() {
    let t = Tree::new();
    record_general(&t.api);

    write_cold_start(&t.api);
    // Run 2 serves only the forward walk, which returns nothing. A prune
    // keyed on "not returned this run" would take all three.
    write_nothing_new(&t.api);

    t.serve();

    run_fetch(&t.out, 0).await;
    let pruned = run_fetch(&t.out, 0).await;

    assert_eq!(pruned, 0, "nothing was re-enumerated, so nothing may go");
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_A.to_string(), TS_B.to_string(), TS_C.to_string()],
        "a forward walk that returned nothing is not evidence of deletion",
    );
}

/// A walk that stopped short must prune nothing.
///
/// Slack signals more pages with `response_metadata.next_cursor`, and the
/// loop also stops when that is absent. A response claiming `has_more`
/// without a cursor therefore ends the walk mid-range — harmless while the
/// only cost was fetching less, and destructive once "absent from the walk"
/// started meaning "deleted". This is that case: the window pass reads one
/// page of a two-page range, and the messages it never reached must stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_walk_prunes_nothing() {
    let t = Tree::new();
    record_general(&t.api);

    write_cold_start(&t.api);
    write_nothing_new(&t.api);
    // The window pass gets a page claiming more, with no cursor to follow.
    write_window(&t.api, json!([msg(TS_A, "a")]), true);

    t.serve();

    run_fetch(&t.out, 0).await;
    let pruned = run_fetch(&t.out, WHOLE_CHANNEL_DAYS).await;

    assert_eq!(pruned, 0, "a walk that stopped short licenses no deletion");
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_A.to_string(), TS_B.to_string(), TS_C.to_string()],
        "B and C were never reached by the walk, so their absence from it \
         says nothing about whether Slack still has them",
    );
}

/// A refresh window deleted every thread reply inside it: history lists a
/// thread's root and never its replies, so the replies were absent from
/// the re-walk and read as deleted — and stayed gone, because the thread's
/// `latest_reply` had not moved and the reply pass skipped it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rewalked_window_keeps_the_replies_of_a_thread_it_lists() {
    let t = Tree::new();
    record_general(&t.api);

    write_cold_start_with_thread(&t.api);
    // Run 2: nothing upstream changed. The window re-serves the same three
    // top-level messages, the root with the same `latest_reply`.
    write_nothing_new(&t.api);
    write_window(
        &t.api,
        json!([thread_root(), msg(TS_B, "b"), msg(TS_C, "c")]),
        false,
    );

    t.serve();

    let everything = vec![
        TS_A.to_string(),
        TS_REPLY_1.to_string(),
        TS_B.to_string(),
        TS_REPLY_2.to_string(),
        TS_C.to_string(),
    ];
    run_fetch(&t.out, 0).await;
    assert_eq!(stored_ts(&t.out), everything, "run 1 mirrors the thread");

    let pruned = run_fetch(&t.out, WHOLE_CHANNEL_DAYS).await;
    assert_eq!(pruned, 0, "nothing was deleted upstream");
    assert_eq!(
        stored_ts(&t.out),
        everything,
        "history never lists a reply, so a reply's absence from it says nothing",
    );
    assert_eq!(
        recorded_latest_reply(&t.out).await.as_deref(),
        Some(TS_REPLY_2),
        "the thread is recorded as current through a reply that is still stored",
    );
}

/// A thread whose root is gone from the re-walked window goes whole: with
/// the root deleted nothing would ever ask for its replies again, so
/// leaving them would strand them, along with a `replies_pages` row that
/// vouches for a thread we no longer hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_root_missing_from_a_rewalked_window_takes_its_replies() {
    let t = Tree::new();
    record_general(&t.api);

    write_cold_start_with_thread(&t.api);
    write_nothing_new(&t.api);
    write_window(&t.api, json!([msg(TS_B, "b"), msg(TS_C, "c")]), false);

    t.serve();

    run_fetch(&t.out, 0).await;
    let pruned = run_fetch(&t.out, WHOLE_CHANNEL_DAYS).await;

    assert_eq!(pruned, 3, "the root and its two replies");
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_B.to_string(), TS_C.to_string()],
        "the replies of a deleted root go with it",
    );
    assert_eq!(recorded_latest_reply(&t.out).await, None);
}

/// A window that takes two pages is judged a page at a time: each page
/// lists a stretch whole, down to its own oldest message, so what it
/// lacks there is deleted with that page. The message the first page
/// ends on is the first page's, and its absence from the second says
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_of_two_pages_deletes_page_by_page() {
    let t = Tree::new();
    record_general(&t.api);

    write_cold_start(&t.api);
    write_nothing_new(&t.api);
    let window = json!({"channel": "C1", "include_all_metadata": "true", "inclusive": "true",
                        "limit": "200", "oldest": SINCE_TS, "latest": TS_C});
    record_call(
        &t.api,
        "conversations.history",
        window.clone(),
        json!({"ok": true, "messages": [msg(TS_C, "c")], "has_more": true,
               "response_metadata": {"next_cursor": "page2"}}),
    )
    .unwrap();
    let mut second_page = window;
    second_page["cursor"] = json!("page2");
    record_call(
        &t.api,
        "conversations.history",
        second_page,
        json!({"ok": true, "messages": [msg(TS_A, "a")], "has_more": false}),
    )
    .unwrap();

    t.serve();

    run_fetch(&t.out, 0).await;
    let pruned = run_fetch(&t.out, WHOLE_CHANNEL_DAYS).await;
    assert_eq!(pruned, 1, "B, which the second page's stretch lacks");
    assert_eq!(stored_ts(&t.out), vec![TS_A.to_string(), TS_C.to_string()],);
}
