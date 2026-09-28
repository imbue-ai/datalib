//! Two-run tests for the config-change adjustments.

use std::path::Path;

use datalib_etl_slack::ingest::FetchOptions;
use datalib_etl_slack::recorded::History;
use serde_json::{json, Value};

use crate::support::{fetch_into, msg, record_general, stored_ts, Tree};

/// `datetime_to_slack_ts` of the corresponding UTC midnight — the exact
/// `oldest` param the downloader emits for each `since` value.
const TS_2023: &str = "1672531200.000000";
const TS_2024: &str = "1704067200.000000";

/// Messages the fixtures serve. `OLD` predates `since: 2024-01-01`, so
/// only a widened `since` can reach it.
const TS_OLD: &str = "1688000000.000000"; // 2023-06-29
const TS_NEW: &str = "1735689600.000000"; // 2025-01-01

/// One `conversations.history` page for `C1`. Each distinct `(oldest,
/// latest, inclusive)` the downloader sends needs its own, which is what
/// makes this test sensitive to the backfill call being made at all.
fn write_history(api: &Path, oldest: &str, latest: Option<&str>, inclusive: bool, messages: Value) {
    History {
        latest,
        inclusive,
        ..History::from("C1", oldest)
    }
    .record(api, messages)
    .unwrap();
}

async fn run_fetch(out: &Path, since: &str) {
    fetch_into(out, |o| FetchOptions {
        since: since.into(),
        ..o
    })
    .await
    .unwrap();
}

/// The headline behavior: widening `since` on an already-synced store
/// fetches the newly-in-scope window below the oldest stored message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn widened_since_backfills_below_oldest_stored_message() {
    let t = Tree::new();
    record_general(&t.api);
    // Run 1, `since: 2024-01-01` — cold start, so `oldest` is the
    // configured since and only the 2025 message is in scope.
    write_history(&t.api, TS_2024, None, true, json!([msg(TS_NEW, "new")]));
    // Run 2, `since: 2023-01-01`. Two calls are expected:
    //   - the forward walk, resuming at the stored resume cursor (exclusive)
    //   - the backfill, `[2023-01-01, oldest_stored]` inclusive
    write_history(&t.api, TS_NEW, None, false, json!([]));
    write_history(
        &t.api,
        TS_2023,
        Some(TS_NEW),
        true,
        json!([msg(TS_OLD, "old")]),
    );

    t.serve();

    run_fetch(&t.out, "2024-01-01").await;
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_NEW.to_string()],
        "run 1 should mirror only the in-scope message",
    );

    run_fetch(&t.out, "2023-01-01").await;
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_OLD.to_string(), TS_NEW.to_string()],
        "run 2 widened `since`, so the older message must be backfilled",
    );
}

/// The steady state: re-running with the same config plans no work, so
/// no backfill call is issued. Guards against the adjustment firing on
/// every run (which would re-walk the whole archive each sync).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_since_issues_no_backfill() {
    let t = Tree::new();
    record_general(&t.api);
    write_history(&t.api, TS_2024, None, true, json!([msg(TS_NEW, "new")]));
    // Only the forward walk is served. A backfill call would 404 against
    // playback and fail the run — which is exactly the assertion.
    write_history(&t.api, TS_NEW, None, false, json!([]));

    t.serve();

    run_fetch(&t.out, "2024-01-01").await;
    run_fetch(&t.out, "2024-01-01").await;

    assert_eq!(stored_ts(&t.out), vec![TS_NEW.to_string()]);
}

/// Narrowing is a no-op: the store is already a superset, and nothing in
/// the pipeline deletes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrowed_since_keeps_existing_messages_and_issues_no_backfill() {
    let t = Tree::new();
    record_general(&t.api);
    write_history(
        &t.api,
        TS_2023,
        None,
        true,
        json!([msg(TS_OLD, "old"), msg(TS_NEW, "new")]),
    );
    write_history(&t.api, TS_NEW, None, false, json!([]));

    t.serve();

    run_fetch(&t.out, "2023-01-01").await;
    run_fetch(&t.out, "2024-01-01").await;

    assert_eq!(
        stored_ts(&t.out),
        vec![TS_OLD.to_string(), TS_NEW.to_string()],
        "narrowing must not drop already-mirrored messages",
    );
}
