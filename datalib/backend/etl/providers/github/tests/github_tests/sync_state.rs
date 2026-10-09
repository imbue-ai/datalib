//! What is owed is what the searches listed minus what the store holds
//! (docs/dev/data_architecture_ingestion.md, "What is left to fetch"). Each test here was written against
//! the old code and watched failing before the mechanism it names was
//! replaced.

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

/// GitHub's search answers at most 1000 results, newest first. A scope
/// with more PRs than that listed the newest 1000 and set its cursor to
/// now, so the rest were never listed: the next run searched from the
/// cursor and found nothing older.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_cut_off_at_githubs_cap_lists_the_rest_next_run() {
    let d = tempdir().unwrap();
    let out = d.path().join("out");
    let pb = tape(&d.path().join("one"), &[1, 2, 3]);
    let (t1, t2, t3) = (
        "2369-04-10T00:00:00Z",
        "2369-04-12T00:00:00Z",
        "2369-04-14T00:00:00Z",
    );
    // The search has three results and answers two: the cap, in
    // miniature.
    serve_search(
        &pb,
        &search_url(SCOPE, &Bounds::default()),
        3,
        &[item(3, t3), item(2, t2)],
    );
    let one_scope = |o: FetchOptions| FetchOptions {
        scopes: vec![SCOPE.to_string()],
        ..o
    };
    run(&out, &pb, one_scope).await.unwrap();
    assert_eq!(stored_prs(&out), [2, 3]);
    assert_eq!(
        problems(&out).await,
        [row("listing:search author:@me", "error")],
        "a search that answered fewer than it has says so"
    );
    assert_eq!(
        coverage(&out).await,
        [(
            "search:author:@me".to_string(),
            "2369-04-15T00:00:00Z".to_string()
        )]
    );

    // What is below the oldest result is still to list, and the search
    // for it is bounded above by that result.
    let below = search_url(
        SCOPE,
        &Bounds {
            lo: None,
            hi: Some(t2.to_string()),
        },
    );
    serve_search(&pb, &below, 2, &[item(2, t2), item(1, t1)]);
    run(&out, &pb, one_scope).await.unwrap();
    assert_eq!(
        stored_prs(&out),
        [1, 2, 3],
        "the PRs the capped search did not reach are listed by the next run"
    );
    assert_eq!(problems(&out).await, []);
}
