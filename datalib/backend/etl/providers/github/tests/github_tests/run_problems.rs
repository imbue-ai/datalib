//! Part of a sync that fails is a `problems` row, not a failed step: the
//! rest of the run goes on, the row goes once the same thing is tried
//! again and works, and a run that was stopped records nothing; what it
//! listed is owed to the next run.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_forge_ingest_common::Bounds;
use datalib_etl_github::ingest::{
    db_path_for, search_url, FetchOptions, RawDb, BASE, DEFAULT_SCOPES,
};
use datalib_etl_web::http::{HttpRequest, HttpService};
use datalib_etl_web::retry::RetryGuard;
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::json;
use tempfile::tempdir;

use crate::support::*;

/// A PR whose own record will not fetch is an error row on it, and the
/// other PR is mirrored all the same. The next run asks for it again
/// although its search, resumed past it, no longer names it — and the
/// row goes. What a search covered reaches the run's pinned clock, not
/// the wall's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_that_would_not_fetch_is_a_problem_until_it_does() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2]);
    fs::remove_file(tape_of(&pb, &format!("{BASE}/repos/{REPO}/pulls/2"))).unwrap();

    run(&out, &pb, |o| o)
        .await
        .expect("one PR failing is not the run failing");
    assert_eq!(stored_prs(&out), [1]);
    assert_eq!(
        problems(&out).await,
        [row(&format!("pull_requests:{REPO}#2"), "error")]
    );
    let pinned = "2369-04-15T00:00:00Z".to_string();
    assert_eq!(
        coverage(&out).await,
        DEFAULT_SCOPES
            .iter()
            .map(|s| (format!("search:{s}"), pinned.clone()))
            .collect::<Vec<_>>(),
    );

    let pb = tape(&d.path().join("two"), &[1, 2]);
    // The searches this run sends are the resumed ones, which list
    // nothing: only the listing the first run stored can bring PR 2 in.
    for scope in DEFAULT_SCOPES {
        tape_of(&pb, &search_url(scope, &resumed()));
    }
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2]);
    assert_eq!(problems(&out).await, [], "the PR fetched this time");
}

/// A scope whose search fails is a `listing:` row naming it; it covers
/// nothing, so the next run searches the same span again, and the row
/// goes when that search works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_that_fails_is_a_problem_until_it_lists() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1]);
    fs::remove_file(tape_of(
        &pb,
        &search_url("mentions:@me", &Bounds::default()),
    ))
    .unwrap();

    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1], "the other scopes listed it");
    assert_eq!(
        problems(&out).await,
        [row("listing:search mentions:@me", "error")]
    );
    let searched: Vec<String> = coverage(&out).await.into_iter().map(|(s, _)| s).collect();
    assert_eq!(searched, ["search:author:@me", "search:commenter:@me"]);

    let pb = tape(&d.path().join("two"), &[1]);
    run(&out, &pb, |o| FetchOptions {
        full_sync: true,
        ..o
    })
    .await
    .unwrap();
    assert_eq!(problems(&out).await, [], "the scope listed this time");
}

/// Raises the stop flag when the run announces how many PRs it will
/// fetch: after discovery, before the first fetch.
struct StopBeforeFetching(StopFlag);

impl ProgressSink for StopBeforeFetching {
    fn set_length(&self, _total: Option<u64>) {
        self.0.request();
    }
}

/// A run stopped between its searches and its fetches has listed PRs it
/// never fetched. What it listed is stored, so the next run fetches it
/// without any search naming it again; replacing the listing rows would
/// clear what the stopped run never re-checked, so it does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_run_keeps_what_it_listed_and_clears_nothing() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1]);
    fs::remove_file(tape_of(
        &pb,
        &search_url("mentions:@me", &Bounds::default()),
    ))
    .unwrap();
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(
        problems(&out).await,
        [row("listing:search mentions:@me", "error")]
    );

    let pb = tape(&d.path().join("two"), &[1, 2]);
    let control = datalib_etl::control::DownloadControl::default();
    let progress = Progress::new(Arc::new(StopBeforeFetching(control.stop.clone())));
    run(&out, &pb, |o| FetchOptions {
        full_sync: true,
        control,
        progress,
        ..o
    })
    .await
    .unwrap();
    assert_eq!(stored_prs(&out), [1], "the stop came before any fetch");
    assert_eq!(
        listed(&out).await.len(),
        2,
        "what the stopped run listed is stored"
    );
    assert_eq!(
        problems(&out).await,
        [row("listing:search mentions:@me", "error")],
        "a stopped run did not re-check the listing"
    );

    // The next run's searches list nothing new; PR 2 is fetched from
    // the listing the stopped run left.
    let pb = tape(&d.path().join("three"), &[1, 2]);
    for scope in DEFAULT_SCOPES {
        tape_of(&pb, &search_url(scope, &resumed()));
    }
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2]);
    assert_eq!(problems(&out).await, []);
}

/// `max_prs` fetches part of what the searches listed. The rest is owed
/// — no row says so: the listing holds it and the store does not — and
/// later runs fetch it first, so a run of capped syncs gets through
/// everything, though no later search lists it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capped_runs_fetch_everything_in_turn() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    let capped = |o: FetchOptions| FetchOptions {
        max_prs: Some(1),
        ..o
    };

    run(&out, &pb, capped).await.unwrap();
    assert_eq!(stored_prs(&out), [1]);
    assert_eq!(listed(&out).await.len(), 3);
    run(&out, &pb, capped).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2]);
    assert_eq!(problems(&out).await, []);
    run(&out, &pb, capped).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2, 3]);
    assert_eq!(problems(&out).await, []);
}

/// A PR GitHub answers 404 for — its repository deleted, or out of this
/// credential's reach — is gone, not failing: it leaves the listing,
/// its failure and its row go, and no later run asks for it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_that_is_gone_stops_being_retried() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2]);
    let pr2 = format!("{BASE}/repos/{REPO}/pulls/2");
    fs::remove_file(tape_of(&pb, &pr2)).unwrap();
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(
        problems(&out).await,
        [row(&format!("pull_requests:{REPO}#2"), "error")]
    );

    let pb = tape(&d.path().join("two"), &[1, 2]);
    let mut not_found = json_response(&json!({"message": "Not Found"}));
    not_found.status = 404;
    write_fixture(
        &pb,
        &HttpRequest::get(HttpService::Github, &pr2),
        &not_found,
    )
    .unwrap();
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1]);
    assert_eq!(problems(&out).await, []);
    assert_eq!(
        query(
            &out,
            "SELECT id, last_error FROM pull_requests_bookkeeping WHERE last_error IS NOT NULL"
        )
        .await,
        [],
        "nothing is left to retry"
    );
    assert_eq!(
        listed(&out).await,
        [(format!("{REPO}#1"), None)],
        "the listing no longer names it"
    );
}

/// The PR's detail page answers 503: retryable, so it spends the retry
/// loop's budget.
fn unavailable(pb: &Path, num: u32) {
    let mut unavailable = json_response(&json!({"message": "Service Unavailable"}));
    unavailable.status = 503;
    write_fixture(
        pb,
        &HttpRequest::get(
            HttpService::Github,
            format!("{BASE}/repos/{REPO}/pulls/{num}"),
        ),
        &unavailable,
    )
    .unwrap();
}

/// When the shared retry loop gives up, the requests stop there — the
/// PRs after it are not each tried and failed — but the run keeps what
/// it fetched: a `phase:` row says it stopped short, what the searches
/// listed stays owed, and the next run fetches the rest and clears the
/// row. A run that gave up did not reach everything, so an earlier
/// run's row stands until a run gets through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_give_up_keeps_what_it_fetched() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let db = RawDb::open(&db_path_for(&out)).await.unwrap();
    datalib_etl::run_problems::collecting(db.pool(), &StopFlag::new(), |found| async move {
        found.listing("search starfleet/defiant", "HTTP 502");
        Ok(())
    })
    .await
    .unwrap();
    db.commit_all("an earlier run").await.unwrap();
    db.close().await;
    let pb = tape(&d.path().join("one"), &[1, 2, 3, 4]);
    unavailable(&pb, 3);
    // One failed request spends the whole budget.
    let quick = Duration::from_millis(1);
    let guard = RetryGuard::new(Duration::from_secs(3600), 1, quick, quick, StopFlag::new());
    datalib_etl_web::retry::scope(guard, run(&out, &pb, |o| o))
        .await
        .expect("a give-up keeps what the run fetched");

    assert_eq!(stored_prs(&out), [1, 2]);
    assert_eq!(
        problems(&out).await,
        [
            row("listing:search starfleet/defiant", "error"),
            row("phase:fetch", "error")
        ]
    );
    assert_eq!(listed(&out).await.len(), 4, "the searches had finished");

    let pb = tape(&d.path().join("two"), &[1, 2, 3, 4]);
    for scope in DEFAULT_SCOPES {
        tape_of(&pb, &search_url(scope, &resumed()));
    }
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2, 3, 4]);
    assert_eq!(problems(&out).await, []);
}

/// A PR that fails every run is tried again, but the ones never tried
/// go before it, so under `max_prs` the rest still get through, one
/// capped run at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_that_always_fails_does_not_starve_the_cap() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    fs::remove_file(tape_of(&pb, &format!("{BASE}/repos/{REPO}/pulls/1"))).unwrap();
    let capped = |o: FetchOptions| FetchOptions {
        max_prs: Some(1),
        ..o
    };

    for _ in 0..3 {
        run(&out, &pb, capped).await.unwrap();
    }
    assert_eq!(stored_prs(&out), [2, 3]);
    assert_eq!(
        problems(&out).await,
        [row(&format!("pull_requests:{REPO}#1"), "error")]
    );
}
