//! What is owed is what the searches listed minus what the store holds
//! (docs/dev/data_architecture_ingestion.md, "What is left to fetch"). Each test here was written against
//! the old code and watched failing before the mechanism it names was
//! replaced.

use std::fs;
use std::path::Path;

use datalib_etl_forge_ingest_common::Bounds;
use datalib_etl_github::ingest::{search_url, FetchOptions, BASE};
use datalib_etl_web::http::{HttpRequest, HttpService};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::*;

const SCOPE: &str = "author:@me";

fn item(num: u64, updated_at: &str) -> Value {
    json!({
        "repository_url": format!("{BASE}/repos/{REPO}"),
        "number": num,
        "updated_at": updated_at,
    })
}

fn serve_search(pb: &Path, url: &str, total_count: usize, items: &[Value]) {
    write_fixture(
        pb,
        &HttpRequest::get(HttpService::Github, url),
        &json_response(&json!({
            "total_count": total_count,
            "incomplete_results": false,
            "items": items,
        })),
    )
    .unwrap();
}

const T1: &str = "2369-04-10T00:00:00Z";
const T2: &str = "2369-04-12T00:00:00Z";
const T3: &str = "2369-04-14T00:00:00Z";
const WHOLE: &str = "..2369-04-15T00:00:00Z";

fn one_scope(o: FetchOptions) -> FetchOptions {
    FetchOptions {
        scopes: vec![SCOPE.to_string()],
        ..o
    }
}

/// The search of everything below `hi`.
fn below(hi: &str) -> String {
    search_url(
        SCOPE,
        &Bounds {
            lo: None,
            hi: Some(hi.to_string()),
        },
    )
}

/// Each `search:` span as `lo..hi`.
async fn spans(out: &Path) -> Vec<(String, String)> {
    query(
        out,
        "SELECT scope, lo || '..' || hi FROM coverage ORDER BY scope, lo",
    )
    .await
}

fn the_scope_whole() -> Vec<(String, String)> {
    vec![(format!("search:{SCOPE}"), WHOLE.to_string())]
}

/// GitHub's search answers at most 1000 results, newest first. A scope
/// with more than that used to list the newest 1000, leave an error row
/// on the Manage screen, and wait for the next run to list the rest. The
/// search carries on below its oldest result instead, as many times as
/// the scope needs, and nothing failed, so no row says it did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_cut_off_at_githubs_cap_carries_on_in_the_same_run() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3, 4]);
    let t0 = "2369-04-08T00:00:00Z";
    // Two results a search: the cap, in miniature. Each search below asks
    // for the oldest result again, in case others share its stamp.
    serve_search(
        &pb,
        &search_url(SCOPE, &Bounds::default()),
        4,
        &[item(4, T3), item(3, T2)],
    );
    serve_search(&pb, &below(T2), 3, &[item(3, T2), item(2, T1)]);
    serve_search(&pb, &below(T1), 2, &[item(2, T1), item(1, t0)]);

    run(&out, &pb, one_scope).await.unwrap();

    assert_eq!(stored_prs(&out), [1, 2, 3, 4]);
    assert_eq!(problems(&out).await, [], "nothing failed");
    assert_eq!(
        spans(&out).await,
        the_scope_whole(),
        "the scope is covered from the beginning of time, so the next run owes nothing below"
    );
}

/// Carrying on stops at the bottom of what the run wants: a run with a
/// refresh window does not walk down to the first pull request ever made.
/// A bound below is only a date, so a search can answer results from
/// under the floor, and those mean it has reached it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_search_carries_on_no_further_than_the_floor() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    // Another scope fills the store, so the next run has a floor.
    run(&out, &pb, |o| FetchOptions {
        scopes: vec!["commenter:@me".to_string()],
        ..o
    })
    .await
    .unwrap();

    let floor = "2369-04-11T00:00:00Z";
    let bounded = |hi: Option<&str>| {
        search_url(
            SCOPE,
            &Bounds {
                lo: Some(floor.to_string()),
                hi: hi.map(str::to_string),
            },
        )
    };
    serve_search(&pb, &bounded(None), 9, &[item(3, T3), item(2, T2)]);
    serve_search(&pb, &bounded(Some(T2)), 9, &[item(2, T2), item(1, floor)]);
    run(&out, &pb, |o| FetchOptions {
        refresh_window_days: 4,
        ..one_scope(o)
    })
    .await
    .unwrap();

    assert_eq!(
        problems(&out).await,
        [],
        "another search would have found no tape, and left a row"
    );
    assert_eq!(
        spans(&out).await[0],
        (
            format!("search:{SCOPE}"),
            format!("{floor}..2369-04-15T00:00:00Z")
        )
    );
}

/// As many results share one `updated_at` as one search will answer, so
/// there is no narrower search to ask. The run ends, with a warning that
/// says what is missing, and the next run asks again: the row stays while
/// that is still so, and goes when it no longer is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_search_that_gets_no_further_ends_with_a_warning_not_a_loop() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    let tied = [item(3, T2), item(2, T2)];
    serve_search(&pb, &search_url(SCOPE, &Bounds::default()), 3, &tied);
    serve_search(&pb, &below(T2), 3, &tied);

    run(&out, &pb, one_scope).await.unwrap();

    assert_eq!(stored_prs(&out), [2, 3], "what it did list is fetched");
    assert_eq!(
        problems(&out).await,
        [row("listing:search author:@me", "warning")],
        "nothing failed, so it is not an error"
    );
    let said = query(out.as_path(), "SELECT scope_key, sample FROM problems").await;
    assert!(said[0].1.contains(T2), "{}", said[0].1);
    assert!(said[0].1.contains("not listed"), "{}", said[0].1);
    assert_eq!(
        spans(&out).await,
        [(
            format!("search:{SCOPE}"),
            format!("{T2}..2369-04-15T00:00:00Z")
        )],
        "what is below the tie is not covered"
    );

    run(&out, &pb, one_scope).await.unwrap();
    assert_eq!(
        problems(&out).await,
        [row("listing:search author:@me", "warning")],
        "each run asks again"
    );

    serve_search(&pb, &below(T2), 3, &[item(3, T2), item(2, T2), item(1, T1)]);
    run(&out, &pb, one_scope).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2, 3]);
    assert_eq!(problems(&out).await, []);
    assert_eq!(spans(&out).await, the_scope_whole());
}

/// A search carrying on can fail like any other. That one is an error,
/// what the search above it covered stays covered, and the next run asks
/// only for what is below. A store an earlier release left after a capped
/// search looks just like this one, row and all, so this is also the
/// proof that such a library catches up in one run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_that_fails_while_carrying_on_keeps_what_was_covered() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    let top = search_url(SCOPE, &Bounds::default());
    serve_search(&pb, &top, 3, &[item(3, T3), item(2, T2)]);

    run(&out, &pb, one_scope).await.unwrap();

    assert_eq!(stored_prs(&out), [2, 3]);
    assert_eq!(
        problems(&out).await,
        [row("listing:search author:@me", "error")]
    );
    assert_eq!(
        spans(&out).await,
        [(
            format!("search:{SCOPE}"),
            format!("{T2}..2369-04-15T00:00:00Z")
        )]
    );

    serve_search(&pb, &below(T2), 2, &[item(2, T2), item(1, T1)]);
    fs::remove_file(tape_of(&pb, &top)).unwrap();
    run(&out, &pb, one_scope).await.unwrap();
    assert_eq!(stored_prs(&out), [1, 2, 3]);
    assert_eq!(problems(&out).await, [], "the row goes");
    assert_eq!(spans(&out).await, the_scope_whole());
}
