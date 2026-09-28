//! Slack has no tombstones and no changes cursor: a deleted message just
//! stops appearing in `conversations.history`. The only way to see that is
//! to re-walk a bounded range and compare, which the trailing
//! `refresh_window_days` pass already does. These are two-run playback
//! tests over that: run 1 mirrors a channel, run 2 serves a refresh window
//! that omits one message, and the question is what happens to our copy.

use std::path::Path;

use datalib_etl_slack::ingest::FetchOptions;
use datalib_etl_slack::recorded::History;
use serde_json::{json, Value};

use crate::support::{fetch_into, msg, record_general, stored_ts, Tree};

/// Far enough back that `refresh_window_days: 3650` covers every message
/// here, so the window pass re-walks the whole channel.
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

/// The cold start: all three messages from `SINCE`.
fn write_cold_start(api: &Path) {
    History::from("C1", SINCE_TS)
        .record(api, json!([msg(TS_A, "a"), msg(TS_B, "b"), msg(TS_C, "c")]))
        .unwrap();
}

/// The forward walk from the watermark, which finds nothing new.
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
    // Run 2: the forward walk from the watermark finds nothing new, then
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

    let pruned = run_fetch(&t.out, 3650).await;
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
/// message below the resume watermark on every run — the forward walk
/// starts at the watermark and returns nothing older, which is
/// indistinguishable from "everything older was deleted" to any check that
/// does not know the walk's bounds.
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
    let pruned = run_fetch(&t.out, 3650).await;

    assert_eq!(pruned, 0, "a walk that stopped short licenses no deletion");
    assert_eq!(
        stored_ts(&t.out),
        vec![TS_A.to_string(), TS_B.to_string(), TS_C.to_string()],
        "B and C were never reached by the walk, so their absence from it \
         says nothing about whether Slack still has them",
    );
}
