//! What is owed is what upstream listed minus what the store holds, and
//! nothing else stands in for it (docs/dev/plans/completed/sync_state.md §6, the
//! Notion rows). Each test here was written against the old code and
//! watched failing before the mechanism it names was replaced.

use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::{fetch, FetchOptions, RawDb};
use datalib_etl_web::interrupt::{self, How};
use datalib_etl_web::retry::{self, RetryGuard};
use serde_json::json;
use tempfile::tempdir;

use crate::support::*;

/// N1. A comments listing that failed was a mark on the page's own row,
/// and the page's next write cleared it before its comments were asked
/// again. A run stopped between the two left the page reading as whole,
/// and no later run asked for its comments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_comments_listing_survives_the_page_being_written_again() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_object(&tape, BRIDGE, EDITED);
    serve_status(&tape, &format!("{BASE}/pages/{BRIDGE}/markdown"), 404);
    serve_status(&tape, &comments_url(BRIDGE), 500);
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        problems(&store).await.len(),
        1,
        "the failed listing is recorded"
    );

    // Upstream now answers; this run is stopped at the request for the
    // comments, after the page has been written again (its 404 body is
    // held from the first run).
    serve_comment_list(&tape, BRIDGE, json!([comment("c1", BRIDGE, None)]));
    let stop = StopFlag::new();
    let fast = std::time::Duration::from_millis(1);
    let guard = RetryGuard::new(
        std::time::Duration::from_secs(3600),
        100,
        fast,
        fast,
        stop.clone(),
    );
    std::env::set_var(datalib_etl_web::http::PLAYBACK_ENV, &tape);
    let db = RawDb::open(&store).await.unwrap();
    let ran = interrupt::run(
        Some(2),
        How::Stop,
        stop.clone(),
        retry::scope(
            guard,
            fetch(FetchOptions {
                subtree_pages: vec![BRIDGE.to_string()],
                control: datalib_etl::control::DownloadControl {
                    stop,
                    ..Default::default()
                },
                ..FetchOptions::new(db.clone())
            }),
        ),
    )
    .await;
    assert_eq!(ran.requests, 2, "the page, and the cut at its comments");
    ran.finished
        .unwrap()
        .expect("a stopped run is a shorter run");
    db.commit_all("test").await.unwrap();
    db.close().await;
    assert!(stored_comments(&store).await.is_empty());

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        stored_comments(&store).await,
        vec![(BRIDGE.to_string(), "c1".to_string())],
        "the comments were still owed, so the next run asked"
    );
    assert!(problems(&store).await.is_empty());
}

/// N2. `max_pages` moved the search mark past the pages it never
/// fetched, with no row saying so; the next run started above them and
/// they were never mirrored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_pages_bounds_the_listing_and_leaves_the_rest_for_the_next_run() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let later = "2026-09-02T00:00:00.000Z";
    serve_page(&tape, BRIDGE, later, "");
    serve_page(&tape, SICKBAY, EDITED, "");
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, later), page(SICKBAY, EDITED)]),
        None,
    );

    run_with(&tape, &store, |o| FetchOptions {
        max_pages: Some(1),
        ..o
    })
    .await
    .unwrap();
    assert_eq!(stored_pages(&store).await, vec![BRIDGE.to_string()]);
    assert_eq!(
        problems(&store).await,
        vec![row("listing:search", "error")],
        "a listing cut off at max_pages says so"
    );

    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        stored_pages(&store).await,
        vec![BRIDGE.to_string(), SICKBAY.to_string()],
        "what the bounded run did not list is listed by the next"
    );
    assert!(problems(&store).await.is_empty());
}

/// N4. The mark was stored to the second and Notion stamps to the
/// millisecond, so `…:00.000Z` sorted below `…:00Z` and a page edited
/// in the same minute as the newest one the last run saw ended the walk
/// before it was listed. Notion reports edit times to the minute, so
/// every page edited in that minute was missed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_edited_in_the_same_minute_as_the_newest_listed_is_still_listed() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), None);
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(stored_pages(&store).await, vec![BRIDGE.to_string()]);

    serve_page(&tape, SICKBAY, EDITED, "");
    serve_search(
        &tape,
        None,
        json!([page(SICKBAY, EDITED), page(BRIDGE, EDITED)]),
        None,
    );
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        stored_pages(&store).await,
        vec![BRIDGE.to_string(), SICKBAY.to_string()]
    );
}

/// N7. A commented block whose fetch failed reached only a debug log
/// line: no row said the thread's anchor was missing, and nothing owed
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_anchor_that_did_not_fetch_is_a_problem_until_it_does() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let block = "1701d000-0000-4000-8000-0000000000bb";
    serve_object(&tape, BRIDGE, EDITED);
    serve_body(&tape, BRIDGE, "Senior staff, report.\n", false);
    serve_comment_list(&tape, BRIDGE, json!([comment("c1", BRIDGE, Some(block))]));

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("comment_anchors:{block}"), "error")]
    );

    serve(
        &tape,
        &format!("{BASE}/blocks/{block}"),
        json!({"object": "block", "id": block, "type": "paragraph",
               "paragraph": {"rich_text": [{"plain_text": "Warp core alignment"}]}}),
    );
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    let db = RawDb::open(&store).await.unwrap();
    let anchors = db.load_comment_anchors().await.unwrap();
    db.close().await;
    assert_eq!(
        anchors.get(block).map(String::as_str),
        Some("Warp core alignment")
    );
}
