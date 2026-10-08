//! Part of a sync that fails is a `problems` row, not a failed step or a
//! log line: the rest of the run goes on, and the row goes once the same
//! thing fetches — which the next run does on its own, because what is
//! not held at its listed stamp is owed.

use std::path::Path;

use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::RawDb;
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::owed;
use datalib_etl_web::synthesize::write_fixture;
use serde_json::json;
use tempfile::tempdir;

use crate::support::*;

/// A body that would not fetch used to be a log line, and the page was
/// stored with its new `last_edited_time`, so no later run asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_did_not_fetch_is_a_problem_until_it_does() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_object(&tape, BRIDGE, EDITED);
    serve_comments(&tape, BRIDGE);

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("page_markdown:{BRIDGE}"), "error")]
    );

    serve_body(&tape, BRIDGE, "Captain's log.\n", false);
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    let db = RawDb::open(&store).await.unwrap();
    let bodies = db.load_page_markdown().await.unwrap();
    db.close().await;
    assert_eq!(
        bodies,
        vec![(BRIDGE.to_string(), "Captain's log.\n".into())]
    );
}

/// An attachment whose bytes did not come back was stamped as fetched.
/// Its signed URL lives only in the response that named it, so the
/// body is read again for the retry, though the page has not moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attachment_that_did_not_fetch_is_retried_through_its_page() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let signed =
        "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/sickbay.png?X-Amz-Signature=abc";
    let slot = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/sickbay.png";
    serve_page(&tape, SICKBAY, EDITED, &format!("![chart]({signed})\n"));

    run(&tape, &store, &[SICKBAY]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(
            &format!("notion_attachments:{SICKBAY}#{slot}"),
            "error"
        )]
    );

    let bytes = HttpResponse {
        status: 200,
        headers: [("content-type".to_string(), "image/png".to_string())].into(),
        body: b"\x89PNG".to_vec(),
        duration_ms: 0,
    };
    write_fixture(
        &tape,
        &HttpRequest::get(HttpService::Notion, signed).plain(),
        &bytes,
    )
    .unwrap();
    run(&tape, &store, &[SICKBAY]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    let db = RawDb::open(&store).await.unwrap();
    assert!(db.blob_exists(slot).await.unwrap());
    db.close().await;
}

/// A comments listing that failed was swallowed as "no comments".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comments_that_did_not_list_are_a_problem_on_their_own_row_until_they_do() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_object(&tape, BRIDGE, EDITED);
    serve_body(&tape, BRIDGE, "Captain's log.\n", false);

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("page_comments:{BRIDGE}"), "error")],
        "the listing's own row, not the page's"
    );

    serve_comments(&tape, BRIDGE);
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}

/// A truncated subtree whose follow-up failed left the body incomplete
/// for good, with nothing but a log line to say so. The body is owed
/// until it comes whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subtree_that_did_not_fetch_is_a_problem_until_it_does() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let hole = "1701d000-0000-4000-8000-0000000000aa";
    let marker = format!(
        "<unknown url=\"https://www.notion.so/x#{}\"/>",
        hole.replace('-', "")
    );
    serve_object(&tape, HOLODECK, EDITED);
    serve_body(&tape, HOLODECK, &format!("Program list\n{marker}\n"), true);
    serve_comments(&tape, HOLODECK);

    run(&tape, &store, &[HOLODECK]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("page_markdown:{HOLODECK}"), "error")]
    );
    let db = RawDb::open(&store).await.unwrap();
    assert!(
        db.load_page_markdown().await.unwrap().is_empty(),
        "an incomplete body is owed, not stored"
    );
    db.close().await;

    serve_body(&tape, hole, "Dixon Hill\n", false);
    run(&tape, &store, &[HOLODECK]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    let db = RawDb::open(&store).await.unwrap();
    let bodies = db.load_page_markdown().await.unwrap();
    db.close().await;
    assert!(bodies[0].1.contains("Dixon Hill"), "{bodies:?}");
}

/// A user that could not be read was only logged, and never asked for
/// again unless a page naming it changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_that_did_not_fetch_is_retried_every_run() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let riker = "1701d000-0000-4000-8000-0000000000bb";
    let mut obj = page(BRIDGE, EDITED);
    obj["created_by"] = json!({"object": "user", "id": riker});
    serve(&tape, &format!("{BASE}/pages/{BRIDGE}"), obj);
    serve_body(&tape, BRIDGE, "", false);
    serve_comments(&tape, BRIDGE);

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("users:{riker}"), "error")]
    );

    serve(
        &tape,
        &format!("{BASE}/users/{riker}"),
        json!({"object": "user", "id": riker, "name": "William Riker"}),
    );
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}

/// A configured root Notion does not have was only a `pages:` row, which
/// does not say the config is what needs fixing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_notion_does_not_have_is_a_config_problem() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_status(&tape, &format!("{BASE}/pages/{SICKBAY}"), 404);

    run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("config:roots:{SICKBAY}"), "warning")],
        "a page Notion does not have is gone, not failed"
    );

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}

/// The bodies the store owes: listed pages not held at their stamp.
async fn owed_bodies(store: &Path) -> Vec<String> {
    let db = RawDb::open(store).await.unwrap();
    let listed = db.pages_listed().await.unwrap();
    let mut ids: Vec<String> = owed::owed(db.pool(), "page_markdown", listed)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.key)
        .collect();
    db.close().await;
    ids.sort();
    ids
}

/// A listed page whose body never comes — Notion answers 404 for it,
/// because the page was deleted or unshared while its body was owed —
/// was fetched again every run. A 404 body is held at the page's stamp,
/// so it is asked for again only once the page is edited; search never
/// reports the deletion (§5), so the page stays as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_will_not_come_is_held_in_search_mode() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let earlier = "2026-08-01T00:00:00.000Z";
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_comments(&tape, SICKBAY);
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, earlier)]),
        None,
    );
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("page_markdown:{SICKBAY}"), "error")]
    );
    assert_eq!(owed_bodies(&store).await, vec![SICKBAY.to_string()]);

    let body = format!("{BASE}/pages/{SICKBAY}/markdown");
    serve_status(&tape, &body, 404);
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), None);
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    assert!(owed_bodies(&store).await.is_empty());

    // Not asked for again: a request now would miss the tape and fail.
    unserve(&tape, &body);
    let third = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(third.official_requests, 1, "the search alone");
    assert_eq!(
        stored_pages(&store).await,
        vec![BRIDGE.to_string(), SICKBAY.to_string()],
        "the ingest deletes nothing"
    );
}

/// The same in roots mode: the page's object answers 404 too, which is
/// a `config:` row and not a failure, and its stored row stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_will_not_come_is_held_in_roots_mode() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_object(&tape, BRIDGE, EDITED);
    serve_comments(&tape, BRIDGE);
    serve_status(&tape, &format!("{BASE}/pages/{BRIDGE}/markdown"), 404);
    serve_object(&tape, SICKBAY, EDITED);
    serve_comments(&tape, SICKBAY);

    run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("page_markdown:{SICKBAY}"), "error")],
        "a body that answers 404 is not a failure; one that did not answer is"
    );
    assert_eq!(owed_bodies(&store).await, vec![SICKBAY.to_string()]);

    serve_status(&tape, &format!("{BASE}/pages/{SICKBAY}"), 404);
    let body = format!("{BASE}/pages/{SICKBAY}/markdown");
    serve_status(&tape, &body, 404);
    run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("config:roots:{SICKBAY}"), "warning")]
    );
    assert!(owed_bodies(&store).await.is_empty());

    unserve(&tape, &body);
    let third = run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(third.official_requests, 2, "the two page objects");
    assert_eq!(
        stored_pages(&store).await,
        vec![BRIDGE.to_string(), SICKBAY.to_string()],
        "the ingest deletes nothing"
    );
}

async fn coverage(store: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(store).await.unwrap();
    let spans = sqlx::query_as("SELECT lo, hi FROM coverage WHERE scope = 'search' ORDER BY lo")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    spans
}

/// When the retry guard gave up, the run failed, and a failed run is not
/// committed: every page it had already fetched was thrown away. Now
/// the listing is durable whatever the bodies did: the pages are
/// stored, the search is covered, and the bodies that did not come are
/// owed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_guard_that_gives_up_keeps_what_it_fetched() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let earlier = "2026-08-01T00:00:00.000Z";
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_status(&tape, &format!("{BASE}/pages/{SICKBAY}/markdown"), 503);
    serve_search(
        &tape,
        None,
        json!([
            page(BRIDGE, EDITED),
            page(HOLODECK, earlier),
            page(SICKBAY, earlier)
        ]),
        None,
    );
    let tick = std::time::Duration::from_millis(1);
    let guard = datalib_etl_web::retry::RetryGuard::new(
        std::time::Duration::from_secs(3600),
        1,
        tick,
        tick,
        datalib_etl::stop::StopFlag::default(),
    );

    datalib_etl_web::retry::scope(guard, run(&tape, &store, &[]))
        .await
        .unwrap();
    assert_eq!(
        stored_pages(&store).await,
        vec![
            BRIDGE.to_string(),
            SICKBAY.to_string(),
            HOLODECK.to_string()
        ],
        "a search result is the page object"
    );
    assert_eq!(
        problems(&store).await,
        vec![row("phase:rate_limit", "error")]
    );
    assert_eq!(
        owed_bodies(&store).await,
        vec![SICKBAY.to_string(), HOLODECK.to_string()]
    );
    assert_eq!(
        coverage(&store).await,
        vec![(String::new(), EDITED.to_string())],
        "the listing reached the end, so the search is covered"
    );

    serve_page(&tape, SICKBAY, earlier, "");
    serve_page(&tape, HOLODECK, earlier, "");
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    assert!(owed_bodies(&store).await.is_empty());
}

/// A credential refused on the first request leaves nothing to keep, and
/// fails the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_refused_from_the_start_fails_the_run() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_status(&tape, &format!("{BASE}/pages/{BRIDGE}"), 401);

    let err = run(&tape, &store, &[BRIDGE]).await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "{err:#}");
}

/// A credential refused part-way through ends the run, as one row, and
/// keeps the pages fetched before it rather than one row per page left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_refused_part_way_keeps_what_it_fetched() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_status(&tape, &format!("{BASE}/pages/{SICKBAY}"), 401);

    run(&tape, &store, &[BRIDGE, SICKBAY, HOLODECK])
        .await
        .unwrap();
    assert_eq!(stored_pages(&store).await, vec![BRIDGE.to_string()]);
    assert_eq!(
        problems(&store).await,
        vec![row("phase:credential", "error")]
    );
}

/// A credential that may not read comments (403) made every page a failure,
/// so every run fetched every page again. It is one row, and it costs
/// one request a run: nothing is held for a listing that was refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comments_the_credential_may_not_read_are_one_row() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    for id in [BRIDGE, SICKBAY] {
        serve_object(&tape, id, EDITED);
        serve_body(&tape, id, "Captain's log.\n", false);
        serve_status(&tape, &comments_url(id), 403);
    }

    let first = run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(first.new_pages, 2);
    // Two objects, two bodies, and comments asked once.
    assert_eq!(first.official_requests, 5);
    assert_eq!(
        problems(&store).await,
        vec![row("listing:comments", "warning")]
    );

    let second = run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(second.skipped_pages, 2);
    assert_eq!(
        second.official_requests, 3,
        "the two page objects, and comments asked once"
    );
    assert_eq!(
        problems(&store).await,
        vec![row("listing:comments", "warning")]
    );
}

/// A search that failed past its first page failed the whole step, and
/// would have moved the resume cursor past what it never read. Now it
/// covers only what it read, and the next run walks the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_cut_short_keeps_its_pages_and_covers_only_what_it_read() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), Some("page-2"));

    let summary = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(summary.new_pages, 1);
    assert_eq!(problems(&store).await, vec![row("listing:search", "error")]);
    assert_eq!(
        coverage(&store).await,
        vec![(EDITED.to_string(), EDITED.to_string())],
        "what was read, and nothing below it"
    );

    serve_search(&tape, Some("page-2"), json!([]), None);
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    assert_eq!(
        coverage(&store).await,
        vec![(String::new(), EDITED.to_string())]
    );
}

/// Search names only what moved since the last walk, so a page whose
/// body failed and has not moved since was never asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_mode_owes_a_body_that_failed() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let earlier = "2026-08-01T00:00:00.000Z";
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_comments(&tape, SICKBAY);
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, earlier)]),
        None,
    );
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("page_markdown:{SICKBAY}"), "error")]
    );

    // Upstream: one newer edit, then only what the search already covers.
    let later = "2026-09-02T00:00:00.000Z";
    serve_page(&tape, TEN_FORWARD, later, "");
    serve_search(
        &tape,
        None,
        json!([page(TEN_FORWARD, later), page(SICKBAY, earlier)]),
        None,
    );
    serve_page(&tape, SICKBAY, earlier, "");
    let second = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(second.listed, 1, "the search stopped at what it had");
    assert!(problems(&store).await.is_empty());
}
