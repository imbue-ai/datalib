//! Part of a sync that fails is a `problems` row, not a failed step: the
//! rest of the run goes on, the row goes once the same thing is tried
//! again and works, and a run that was stopped records nothing and moves
//! no cursor.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::http::{fixture_key, HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::retry::RetryGuard;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{json_response, write_fixture, Synthesizer};
use datalib_etl_github::ingest::{
    block_on_load_all, db_path_for, fetch, search_url, FetchOptions, FetchSummary, RawDb, BASE,
    DEFAULT_SCOPES, ENTITY_PR, ENTITY_SELF,
};
use datalib_etl_github::synthesize::GithubSynth;
use serde_json::{json, Map};
use tempfile::tempdir;

const REPO: &str = "enterprise-d/holodeck";
/// What the TNG clock's runs stamp a cursor with, as search takes it.
const SINCE: &str = "2369-04-15";

/// A playback tree for an account on PRs `prs` of [`REPO`], none with
/// comments, captured at the TNG clock — so it also answers the searches
/// a run resumed from that moment sends, with nothing.
fn tape(dir: &Path, prs: &[u64]) -> PathBuf {
    let api = dir.join("events");
    fs::create_dir_all(&api).unwrap();
    let mut k = Map::new();
    k.insert("user_id".into(), json!(17010001));
    let mut me = make_record(k, json!({"id": 17010001, "login": "jlpicard"}));
    me["_recorded_at"] = json!(crate::tng_now().to_rfc3339_secs());
    diff_and_save(&api, ENTITY_SELF, &[me], &HashMap::new(), |r| r.to_string()).unwrap();
    for num in prs {
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(REPO));
        k.insert("pr_number".into(), json!(num));
        let pr = make_record(
            k,
            json!({"number": num, "title": "Safety protocols", "state": "open"}),
        );
        diff_and_save(&api, ENTITY_PR, &[pr], &HashMap::new(), |r| r.to_string()).unwrap();
    }
    let pb = dir.join("playback");
    GithubSynth::new(&api).synthesize(&pb).unwrap();
    pb
}

fn tape_of(pb: &Path, url: &str) -> PathBuf {
    let path = pb
        .join("github")
        .join(fixture_key(&HttpRequest::get(HttpService::Github, url)));
    assert!(
        path.is_file(),
        "no tape for {url}; if the request shape changed, this test would \
         stage nothing and pass for the wrong reason"
    );
    path
}

async fn run(
    out: &Path,
    pb: &Path,
    tweak: impl FnOnce(FetchOptions) -> FetchOptions,
) -> Result<FetchSummary, String> {
    std::env::set_var(PLAYBACK_ENV, pb);
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let opts = FetchOptions {
        refresh_window_days: 0,
        ..FetchOptions::new(db.clone(), crate::tng_now())
    };
    let summary = fetch(tweak(opts)).await.map_err(|e| format!("{e:#}"));
    // As the processor does: a run that fails commits nothing, and the
    // next open of the store drops what it wrote.
    if summary.is_ok() {
        db.commit_all("test").await.unwrap();
    }
    db.close().await;
    summary
}

async fn query(out: &Path, sql: &'static str) -> Vec<(String, String)> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows = sqlx::query_as(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    rows
}

async fn problems(out: &Path) -> Vec<(String, String)> {
    query(
        out,
        "SELECT scope_key, severity FROM problems ORDER BY scope_key",
    )
    .await
}

async fn cursors(out: &Path) -> Vec<(String, String)> {
    query(
        out,
        "SELECT scope, last_seen_at_utc FROM sync_scope_state ORDER BY scope",
    )
    .await
}

fn stored_prs(out: &Path) -> Vec<u32> {
    let raw = block_on_load_all(&db_path_for(out)).unwrap();
    raw.pull_requests.iter().map(|p| p.pr_number).collect()
}

fn row(key: &str, severity: &str) -> (String, String) {
    (key.to_string(), severity.to_string())
}

/// A PR whose own record will not fetch is an error row on it, and the
/// other PR is mirrored all the same. The next run asks for it again
/// although its search, resumed past it, no longer names it — and the
/// row goes. The cursors are the run's pinned clock, not the wall's.
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
    let pinned = "2369-04-15T00:00:00.000000+00:00".to_string();
    assert_eq!(
        cursors(&out).await,
        DEFAULT_SCOPES
            .iter()
            .map(|s| (s.to_string(), pinned.clone()))
            .collect::<Vec<_>>(),
    );

    let pb = tape(&d.path().join("two"), &[1, 2]);
    // The searches this run sends are the resumed ones, which list
    // nothing: only the retry can bring PR 2 in.
    for scope in DEFAULT_SCOPES {
        tape_of(&pb, &search_url(scope, Some(SINCE)));
    }
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2]);
    assert_eq!(problems(&out).await, [], "the PR fetched this time");
}

/// A scope whose search fails is a `listing:` row naming it; its cursor
/// stays where it was so the next run searches the same span again, and
/// the row goes when that search works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_that_fails_is_a_problem_until_it_lists() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1]);
    fs::remove_file(tape_of(&pb, &search_url("mentions:@me", None))).unwrap();

    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1], "the other scopes listed it");
    assert_eq!(
        problems(&out).await,
        [row("listing:search mentions:@me", "error")]
    );
    let searched: Vec<String> = cursors(&out).await.into_iter().map(|(s, _)| s).collect();
    assert_eq!(searched, ["author:@me", "commenter:@me"]);

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
/// never fetched. Moving the cursors would put them behind every later
/// search, and replacing the listing rows would clear what it never
/// re-checked: it does neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_run_moves_no_cursor_and_clears_nothing() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1]);
    fs::remove_file(tape_of(&pb, &search_url("mentions:@me", None))).unwrap();
    run(&out, &pb, |o| o).await.unwrap();
    let before = cursors(&out).await;
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
    assert_eq!(cursors(&out).await, before);
    assert_eq!(
        problems(&out).await,
        [row("listing:search mentions:@me", "error")],
        "a stopped run did not re-check the listing"
    );
}

/// `max_prs` fetches part of what the searches listed. The rest is owed
/// — a warning on each — and later runs fetch it first, so a run of
/// capped syncs gets through everything, though no later search lists
/// it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capped_runs_fetch_everything_in_turn() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    let capped = |o: FetchOptions| FetchOptions {
        max_prs: Some(1),
        ..o
    };
    let owed = |n: u32| row(&format!("pull_requests:{REPO}#{n}"), "warning");

    run(&out, &pb, capped).await.unwrap();
    assert_eq!(stored_prs(&out), [1]);
    run(&out, &pb, capped).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2]);
    assert_eq!(problems(&out).await, [owed(3)]);
    run(&out, &pb, capped).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2, 3]);
    assert_eq!(problems(&out).await, []);
}

/// A PR GitHub answers 404 for — its repository deleted, or out of this
/// credential's reach — is gone, not failing: its failure and its row
/// go, and no later run asks for it again.
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
/// it fetched: a `phase:` row says it stopped short, the cursors stay,
/// and the next run fetches the rest and clears the row. A run that gave
/// up did not reach everything, so an earlier run's row stands until a
/// run gets through.
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
    datalib_etl::retry::scope(guard, run(&out, &pb, |o| o))
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
    assert_eq!(cursors(&out).await, []);

    let pb = tape(&d.path().join("two"), &[1, 2, 3, 4]);
    run(&out, &pb, |o| o).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2, 3, 4]);
    assert_eq!(problems(&out).await, []);
}

/// A PR that fails every run is tried every run, but does not count
/// against `max_prs`: the rest still get through, one capped run at a
/// time.
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
