//! GitHub's per-PR comment/review endpoints each return that PR's whole
//! child list, so a child we hold that the list stops naming was deleted.
//! These tests pin both halves of acting on that: the prune itself, and the
//! refusal to prune when the list request failed — where an empty result is
//! indistinguishable from "everything was deleted".

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::http::{fixture_key, HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_github::download::{
    block_on_load_all, db_path_for, fetch, FetchOptions, ENTITY_ISSUE_COMMENT, ENTITY_PR,
    ENTITY_SELF,
};
use datalib_etl_github::synthesize::GithubSynth;
use serde_json::{json, Map, Value};
use tempfile::tempdir;
use tokio::sync::Mutex;

/// `PLAYBACK_ENV` is process-global; these tests must not overlap.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

const REPO: &str = "octocat/hello";
const NUM: u64 = 7;

fn write_event(api: &Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

/// An event store holding the PR plus whichever issue-comment ids are
/// named. Two of these, differing only in that list, is how a deletion is
/// staged.
fn build_events(api: &Path, comment_ids: &[i64]) {
    fs::create_dir_all(api).unwrap();
    let mut k = Map::new();
    k.insert("user_id".into(), json!(42));
    write_event(api, ENTITY_SELF, k, json!({"id": 42, "login": "octocat"}));

    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(REPO));
    k.insert("pr_number".into(), json!(NUM));
    write_event(
        api,
        ENTITY_PR,
        k,
        json!({
            "number": NUM,
            "title": "T",
            "state": "open",
            "html_url": format!("https://github.com/{REPO}/pull/{NUM}"),
            "head": {"sha": "abc", "ref": "br"},
            "base": {"sha": "def", "ref": "main"},
        }),
    );

    for id in comment_ids {
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(REPO));
        k.insert("pr_number".into(), json!(NUM));
        k.insert("comment_id".into(), json!(id));
        write_event(
            api,
            ENTITY_ISSUE_COMMENT,
            k,
            json!({"id": id, "body": "hi", "user": {"login": "alice"}}),
        );
    }
}

async fn run(out_db: &Path) -> usize {
    fetch(FetchOptions {
        db_path: out_db.to_path_buf(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..FetchOptions::default()
    })
    .await
    .unwrap()
    .pruned
}

fn stored_comment_ids(out_db: &Path) -> Vec<i64> {
    let raw = block_on_load_all(&db_path_for(out_db)).expect("load db");
    let mut ids: Vec<i64> = raw
        .issue_comments
        .iter()
        .filter_map(|c| c.payload.get("id").and_then(|v| v.as_i64()))
        .collect();
    ids.sort();
    ids
}

/// A comment GitHub stops listing is a comment its author deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_comment_dropped_from_the_listing_is_deleted() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("out.doltlite_db");

    let api1 = d.path().join("events1");
    let pb1 = d.path().join("pb1");
    build_events(&api1, &[101, 102]);
    GithubSynth::new(&api1).synthesize(&pb1).unwrap();
    std::env::set_var(PLAYBACK_ENV, &pb1);
    run(&out_db).await;
    assert_eq!(stored_comment_ids(&out_db), vec![101, 102], "both mirrored");

    // Second tape: comment 102 is gone from the PR's listing.
    let api2 = d.path().join("events2");
    let pb2 = d.path().join("pb2");
    build_events(&api2, &[101]);
    GithubSynth::new(&api2).synthesize(&pb2).unwrap();
    std::env::set_var(PLAYBACK_ENV, &pb2);
    let pruned = run(&out_db).await;

    assert_eq!(pruned, 1, "the run must report the deletion it acted on");
    assert_eq!(
        stored_comment_ids(&out_db),
        vec![101],
        "GitHub served this PR's whole comment list and 102 was not in it",
    );
}

/// The guard, and the reason the download path handles the error instead of
/// `unwrap_or_default`-ing it: a failed list request yields an empty `Vec`,
/// which is byte-identical to a PR whose comments were all deleted.
/// Deleting on that would empty a PR's history on a transient 500.
///
/// Note what the previous test establishes alongside it: a listing that
/// *succeeds* and returns `[]` does prune everything, and that is correct.
/// So the two cases really are told apart by the request's outcome and
/// nothing else — which is exactly why the outcome cannot be discarded.
///
/// Staged by deleting the one playback fixture for the comments endpoint,
/// so that request — and only that request — misses and errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_listing_prunes_nothing() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let out_db = d.path().join("out.doltlite_db");

    let api = d.path().join("events");
    let pb = d.path().join("pb");
    build_events(&api, &[101, 102]);
    GithubSynth::new(&api).synthesize(&pb).unwrap();
    std::env::set_var(PLAYBACK_ENV, &pb);
    run(&out_db).await;
    assert_eq!(stored_comment_ids(&out_db), vec![101, 102]);

    // Same tape, minus the comments listing.
    let url = format!("https://api.github.com/repos/{REPO}/issues/{NUM}/comments?per_page=100");
    let key = fixture_key(&HttpRequest::get(HttpService::Github, &url));
    let fixture = pb.join("github").join(&key);
    assert!(
        fixture.is_file(),
        "expected the comments-listing fixture at {}; if the request shape \
         changed, this test is no longer staging a failure and would pass \
         for the wrong reason",
        fixture.display()
    );
    fs::remove_file(&fixture).unwrap();

    let pruned = run(&out_db).await;

    assert_eq!(pruned, 0, "a request that failed licenses no deletion");
    assert_eq!(
        stored_comment_ids(&out_db),
        vec![101, 102],
        "both comments must survive a listing we could not read",
    );
}
