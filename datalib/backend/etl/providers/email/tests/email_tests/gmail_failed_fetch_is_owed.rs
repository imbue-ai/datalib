//! A message the listing named but `messages.get` would not return.
//!
//! Three outcomes that look identical at the call site and are not. A 404
//! means the message was deleted since it was listed: there is nothing to
//! come back for. Any other definitive failure means the message still
//! exists and is still wanted: it stays listed and unfetched, which is
//! what makes the next run ask for it again, with the attempts counted
//! on it. The cursor does not wait for it — a message that never fetches
//! once held the cursor for good, and every run walked the whole mailbox
//! again. And a transient failure that outlasts the retry loop's give-up
//! bounds ends the fetch: nothing after it would fare better, and what
//! was fetched before is kept.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_web::http::HttpResponse;
use datalib_etl_web::retry::{self, RetryGuard};
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, put_gmail_response, Mirror,
};

/// Fetched first: the newer id.
const GOOD: &str = "18c9f2a1b2c3d502";
const BAD: &str = "18c9f2a1b2c3d501";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_that_will_not_fetch_is_owed_and_does_not_hold_the_cursor() {
    let m = mirror_with_bad_status(400);
    let first = run(&m)
        .await
        .expect("a failed message does not fail the run");
    assert_eq!(first.emails_upserted, 1, "the good message still lands");
    assert_eq!(first.messages_failed, 1, "{first:?}");
    let failed = [format!("listed_messages:{BAD}")];
    assert_eq!(
        problems(&m).await,
        failed,
        "the message that failed is a row a person can see"
    );
    assert_eq!(
        cursor(&m).await.as_deref(),
        Some("9001"),
        "the cursor waited on a message that would not fetch",
    );

    // Nothing changed upstream, and nothing lists the message again: it
    // is asked for because the store does not hold it.
    let second = run(&m).await.expect("second run");
    assert!(second.walked.is_empty(), "{second:?}");
    assert_eq!(second.messages_failed, 1, "{second:?}");
    assert_eq!(attempts(&m, BAD).await, 2);
    assert_eq!(problems(&m).await, failed);

    put_gmail(
        &m.playback,
        &gmail_get_url(BAD),
        &gmail_message(BAD, &["INBOX"], "recovered"),
    );
    let third = run(&m).await.expect("third run");
    assert_eq!(third.emails_upserted, 1, "{third:?}");
    assert_eq!(m.gmail_ids().await, ids(&[GOOD, BAD]));
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
    assert_eq!(
        attempts(&m, BAD).await,
        3,
        "the sidecar counts every attempt, the fetch that worked included"
    );
    assert!(
        last_error(&m, BAD).await.is_none(),
        "the failure goes with the fetch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_is_a_deletion() {
    let m = mirror_with_bad_status(404);
    let summary = run(&m).await.expect("a deletion does not fail the run");
    assert_eq!(summary.emails_upserted, 1);
    assert_eq!(summary.messages_failed, 0, "{summary:?}");
    assert!(problems(&m).await.is_empty());
    let listed: Vec<String> = m
        .read(|db| async move {
            sqlx::query_scalar("SELECT id FROM listed_messages")
                .fetch_all(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(listed, [GOOD], "a message Gmail no longer has is not owed");
}

/// Google's 500 `backendError` is retried with backoff, and a message
/// that never stops answering it exhausts the run's give-up bounds. The
/// fetch must then stop rather than walk on to fail every remaining id
/// one attempt at a time. That was once the run's error, and a failed
/// run commits nothing: the messages fetched before it went with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_failure_that_outlasts_the_retries_ends_the_fetch_and_keeps_what_it_fetched() {
    // Two attempts, no wait between them: the bound, not the clock, is
    // what ends the retrying.
    let fast = Duration::from_millis(1);
    let guard = RetryGuard::new(
        Duration::from_secs(3600),
        2,
        fast,
        fast,
        datalib_etl::stop::StopFlag::default(),
    );
    let m = mirror_with_bad_status(500);
    let summary = retry::scope(guard, run(&m))
        .await
        .expect("a fetch that gave up keeps the run's work");
    assert_eq!(summary.emails_upserted, 1, "{summary:?}");
    assert_eq!(m.gmail_ids().await, ids(&[GOOD]));
    assert_eq!(problems(&m).await, ["phase:messages.get"]);

    put_gmail(
        &m.playback,
        &gmail_get_url(BAD),
        &gmail_message(BAD, &["INBOX"], "recovered"),
    );
    run(&m).await.expect("second run");
    assert_eq!(m.gmail_ids().await, ids(&[GOOD, BAD]));
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

async fn run(m: &Mirror) -> anyhow::Result<FetchSummary> {
    m.run(|db| gmail_api::fetch(FetchOptions::new(db))).await
}

async fn cursor(m: &Mirror) -> Option<String> {
    m.read(|db| async move {
        sqlx::query_scalar("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind("gmail:t@example.test:historyId")
            .fetch_optional(db.pool())
            .await
            .expect("read the cursor")
    })
    .await
}

async fn problems(m: &Mirror) -> Vec<String> {
    m.read(|db| async move {
        sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .expect("read the problems")
    })
    .await
}

/// How many times `id` has been asked for.
async fn attempts(m: &Mirror, id: &'static str) -> i64 {
    m.read(|db| async move {
        sqlx::query_scalar(
            "SELECT coalesce(sum(attempt_count), 0) FROM listed_messages_bookkeeping WHERE id = ?",
        )
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("read the attempts")
    })
    .await
}

/// What the last attempt on `id` failed with, if it failed.
async fn last_error(m: &Mirror, id: &'static str) -> Option<String> {
    m.read(|db| async move {
        sqlx::query_scalar("SELECT last_error FROM listed_messages_bookkeeping WHERE id = ?")
            .bind(id)
            .fetch_one(db.pool())
            .await
            .expect("read the last error")
    })
    .await
}

/// A mailbox of one good message and one that answers `bad_status`, and
/// a history that says nothing changed since the first run.
fn mirror_with_bad_status(bad_status: u16) -> Mirror {
    let m = Mirror::new();
    let playback = &m.playback;
    put_gmail_account(playback, "9001", json!([inbox_label()]));
    put_gmail(
        playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": GOOD }, { "id": BAD }] }),
    );
    put_gmail(
        playback,
        &gmail_history_url("9001"),
        &json!({ "historyId": "9001" }),
    );
    put_gmail(
        playback,
        &gmail_get_url(GOOD),
        &gmail_message(GOOD, &["INBOX"], "kept"),
    );
    put_gmail_response(
        playback,
        &gmail_get_url(BAD),
        &HttpResponse {
            status: bad_status,
            headers: BTreeMap::new(),
            body: b"{\"error\":{\"message\":\"nope\"}}".to_vec(),
            duration_ms: 0,
        },
    );
    m
}
