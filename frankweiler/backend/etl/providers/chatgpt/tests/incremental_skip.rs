//! Two-pass resume / skip-check round-trip.
//!
//! Regression guard for the format-mismatch bug (commit 1fc3ee8, then
//! reintroduced in the Rust port): the `/conversations` listing reports
//! `update_time` as an ISO-8601 string while `/conversation/{id}`
//! reports it as a Unix-epoch float. The extract path stores the detail
//! float, so a naive byte-for-byte comparison against the listing string
//! never matches and every already-downloaded conversation gets
//! re-fetched — defeating incremental resume.
//!
//! This test fetches once, then fetches *again* against the same DB and
//! the same playback fixtures (whose listing uses the ISO shape and
//! whose detail uses the float shape, exactly like the live API) and
//! asserts the second pass skips everything.
//!
//! Lives in its own integration-test file — and thus its own Bazel
//! `rust_test` target / process — so the process-wide
//! `FRANKWEILER_HTTP_PLAYBACK` env var can't race other tests.

use std::fs;
use std::time::Duration;

use chrono::DateTime;
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_chatgpt::extract::{fetch, FetchOptions};
use frankweiler_etl_chatgpt::synthesize::ChatgptSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_json(path: &std::path::Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

/// The ISO-8601 string the listing endpoint reports for a detail-side
/// epoch float — microseconds, explicit `+00:00`, matching the live API.
fn iso_for_epoch(epoch: f64) -> String {
    let micros = (epoch * 1_000_000.0).round() as i64;
    DateTime::from_timestamp_micros(micros)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string()
}

async fn run_fetch(out_db: &std::path::Path) -> frankweiler_etl_chatgpt::extract::FetchSummary {
    run_fetch_since(out_db, None).await
}

async fn run_fetch_since(
    out_db: &std::path::Path,
    since: Option<&str>,
) -> frankweiler_etl_chatgpt::extract::FetchSummary {
    fetch(FetchOptions {
        db_path: out_db.to_path_buf(),
        max_pages: None,
        limit: None,
        sleep_between: Duration::ZERO,
        since: since.map(String::from),
        conv_uuids: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_sync_skips_already_downloaded_conversations() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out_snapshot.doltlite_db");

    // Two conversations. The listing carries `update_time` as an ISO
    // string (live-API shape); each detail carries the *same instant* as
    // an epoch float. The skip-check must reconcile the two shapes.
    let epoch_a = 1_710_959_331.420159_f64;
    let epoch_b = 1_711_050_274.768892_f64;
    write_json(
        &api.join("me.json"),
        &json!({"id": "u-1", "email": "x@y.test"}),
    );
    let listing = json!([
        {"id": "c-a", "update_time": iso_for_epoch(epoch_a), "title": "A"},
        {"id": "c-b", "update_time": iso_for_epoch(epoch_b), "title": "B"},
    ]);
    write_json(&api.join("conversations.json"), &listing);
    write_json(
        &api.join("conversations/c-a.json"),
        &json!({"id": "c-a", "update_time": epoch_a, "mapping": {}, "title": "A"}),
    );
    write_json(
        &api.join("conversations/c-b.json"),
        &json!({"id": "c-b", "update_time": epoch_b, "mapping": {}, "title": "B"}),
    );

    ChatgptSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    // First sync: both conversations are new and get fetched.
    let first = run_fetch(&out_db).await;
    assert_eq!(first.fetched, 2, "first sync should fetch both convs");
    assert_eq!(first.skipped, 0);
    assert_eq!(first.errors, 0);

    // Second sync against the same DB: nothing changed upstream, so the
    // ISO listing values must reconcile with the stored float values and
    // both conversations are recognized as up-to-date — zero re-fetches.
    let second = run_fetch(&out_db).await;
    assert_eq!(
        second.fetched, 0,
        "second sync re-fetched already-downloaded convs (skip-check format mismatch)"
    );
    assert_eq!(
        second.skipped, 2,
        "both convs should be skipped as up-to-date"
    );
    assert_eq!(second.errors, 0);

    // `since` scoping against a fresh db: with a cutoff between the two
    // conversations' update_times (a: 2024-03-20, b: 2024-03-21), only
    // the newer c-b is fetched; c-a is out of scope.
    let since_db = d.path().join("out_since.doltlite_db");
    let scoped = run_fetch_since(&since_db, Some("2024-03-21")).await;
    assert_eq!(scoped.fetched, 1, "only c-b is at/after the cutoff");
    assert_eq!(scoped.out_of_scope, 1, "c-a predates the cutoff");
    assert_eq!(scoped.skipped, 0);
    assert_eq!(scoped.errors, 0);

    // Moving `since` further back backfills the newly-in-scope c-a as
    // missing while the already-fetched c-b classifies up to date.
    let backfill = run_fetch_since(&since_db, Some("2024-03-01")).await;
    assert_eq!(backfill.fetched, 1, "c-a backfills once in scope");
    assert_eq!(backfill.skipped, 1, "c-b is already up to date");
    assert_eq!(backfill.out_of_scope, 0);
    assert_eq!(backfill.errors, 0);

    // Early-stop pagination: 101 conversations — one recent, 100 older
    // than the cutoff. The listing is newest-first, so page 1 is the
    // recent conv + 99 old ones and page 2 holds the last old one. The
    // page-1 tail is already past the cutoff, so the walk must stop
    // without ever requesting page 2. (Same test function as above —
    // not a separate #[tokio::test] — so the process-wide PLAYBACK_ENV
    // re-point below can't race a concurrently running test.)
    let api2 = d.path().join("input_snapshot_paged");
    let playback2 = d.path().join("playback_paged");
    let paged_db = d.path().join("out_paged.doltlite_db");

    let epoch_new = 1_711_050_274.0_f64; // 2024-03-21
    let epoch_old = 1_700_000_000.0_f64; // 2023-11-14
    write_json(
        &api2.join("me.json"),
        &json!({"id": "u-1", "email": "x@y.test"}),
    );
    let mut paged_listing = vec![json!({
        "id": "c-new", "update_time": iso_for_epoch(epoch_new), "title": "New",
    })];
    for i in 0..100 {
        // Strictly descending update_times, matching order=updated.
        paged_listing.push(json!({
            "id": format!("c-old-{i}"),
            "update_time": iso_for_epoch(epoch_old - i as f64),
            "title": format!("Old {i}"),
        }));
    }
    write_json(
        &api2.join("conversations.json"),
        &Value::Array(paged_listing),
    );
    write_json(
        &api2.join("conversations/c-new.json"),
        &json!({"id": "c-new", "update_time": epoch_new, "mapping": {}, "title": "New"}),
    );
    ChatgptSynth::new(&api2).synthesize(&playback2).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback2);

    let paged = run_fetch_since(&paged_db, Some("2024-03-01")).await;
    // Only page 1 (100 items) was listed; page 2's single old item was
    // never requested. Without the cutoff stop the listing would be 101.
    assert_eq!(paged.listing, 100, "walk should stop after page 1");
    assert_eq!(paged.fetched, 1, "only c-new is in scope");
    assert_eq!(paged.out_of_scope, 99, "page-1 old items are scoped out");
    assert_eq!(paged.errors, 0);
}
