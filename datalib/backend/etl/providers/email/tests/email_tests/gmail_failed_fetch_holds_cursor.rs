//! A message the enumeration named but `messages.get` would not return.
//!
//! Three outcomes that look identical at the call site and are not. A 404
//! means the message was deleted between the list and the get: there is
//! nothing to come back for, so the run is complete and the cursor may
//! advance. Any other definitive failure means the message still exists
//! and we still want it — and because storing the cursor makes the next
//! run incremental, `history.list` would only name what *changed*, so a
//! message that merely failed to fetch would never be named again. And a
//! transient failure that outlasts the retry loop's give-up bounds ends
//! the run: nothing after it would fare better.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use datalib_etl::http::HttpResponse;
use datalib_etl::retry::{self, RetryGuard};
use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_list_url, gmail_message, inbox_label, put_gmail, put_gmail_account,
    put_gmail_response, Mirror,
};

const GOOD: &str = "18c9f2a1b2c3d501";
const BAD: &str = "18c9f2a1b2c3d502";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_holds_the_cursor_and_a_deletion_does_not() {
    // One test, two scenarios in sequence: `PLAYBACK_ENV` is
    // process-global, so as separate `#[tokio::test]`s they would race
    // and clear each other's fixture root mid-request.
    a_definitive_failure_holds_the_cursor().await;
    a_404_is_a_deletion_and_lets_the_cursor_advance().await;
    a_transient_failure_that_outlasts_the_retries_ends_the_run().await;
}

/// The regression this file exists for. A 400 is not in the transport's
/// retryable set, so it reaches the fetch loop on the first attempt.
async fn a_definitive_failure_holds_the_cursor() {
    let (summary, cursor) = run_with_bad_status(400).await;
    let summary = summary.expect("a failed message does not fail the run");

    assert_eq!(summary.emails_upserted, 1, "the good message still lands");
    assert_eq!(
        summary.messages_failed, 1,
        "the failure must be counted, not swallowed: {summary:?}",
    );
    assert_eq!(
        cursor, None,
        "the cursor advanced past a message this run never fetched — the \
         next run goes incremental and will never name it again",
    );
}

/// The other half, and the reason holding the cursor cannot simply be
/// unconditional: a deleted message is not work left undone.
async fn a_404_is_a_deletion_and_lets_the_cursor_advance() {
    let (summary, cursor) = run_with_bad_status(404).await;
    let summary = summary.expect("a deletion does not fail the run");

    assert_eq!(summary.emails_upserted, 1);
    assert_eq!(
        summary.messages_failed, 0,
        "a deletion is not a failure: {summary:?}",
    );
    assert_eq!(
        cursor.as_deref(),
        Some("9001"),
        "nothing was left undone, so the run may record where it got to",
    );
}

/// Google's 500 `backendError` is retried with backoff, and a message
/// that never stops answering it exhausts the run's give-up bounds. The
/// run must then stop rather than walk on to fail every remaining id
/// one attempt at a time; the cursor stays held so the next run resumes.
async fn a_transient_failure_that_outlasts_the_retries_ends_the_run() {
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
    let (summary, cursor) = retry::scope(guard, run_with_bad_status(500)).await;

    let err = summary.expect_err("a run whose retries gave up must fail");
    assert!(
        format!("{err:#}").contains("gave up retrying"),
        "the error must say the retry loop gave up: {err:#}",
    );
    assert_eq!(
        cursor, None,
        "a run that stopped early may not record a cursor"
    );
}

/// Mirrors one good message and one that answers `bad_status`. Returns
/// the fetch's result and the stored `historyId` cursor, if any.
async fn run_with_bad_status(bad_status: u16) -> (anyhow::Result<FetchSummary>, Option<String>) {
    let m = Mirror::new();
    write_fixtures(&m.playback, bad_status);

    let summary = m.run(|db| gmail_api::fetch(FetchOptions::new(db))).await;
    let cursor = m
        .read(|db| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?",
            )
            .bind("gmail:t@example.test:historyId")
            .fetch_optional(db.pool())
            .await
            .expect("read the cursor")
        })
        .await;
    (summary, cursor)
}

fn write_fixtures(playback: &Path, bad_status: u16) {
    put_gmail_account(playback, "9001", json!([inbox_label()]));
    put_gmail(
        playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": GOOD }, { "id": BAD }] }),
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
}
