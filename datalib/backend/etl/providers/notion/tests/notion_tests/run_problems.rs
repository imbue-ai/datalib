//! Part of a sync that fails is a `problems` row, not a failed step or a
//! log line: the rest of the run goes on, and the row goes once the same
//! thing fetches — which means a later run has to try it again, though
//! upstream has not moved it.

use std::path::Path;

use datalib_etl::http::{fixture_key, HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_notion::ingest::official::{BASE, PAGE_SIZE};
use datalib_etl_notion::ingest::{fetch, FetchOptions, FetchSummary, RawDb};
use serde_json::{json, Value};
use tempfile::tempdir;

const BRIDGE: &str = "1701d000-0000-4000-8000-000000000001";
const SICKBAY: &str = "1701d000-0000-4000-8000-000000000002";
const HOLODECK: &str = "1701d000-0000-4000-8000-000000000003";
const TEN_FORWARD: &str = "1701d000-0000-4000-8000-000000000004";
const EDITED: &str = "2026-09-01T00:00:00.000Z";

fn get(url: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Notion, url).header("Accept", "application/json")
}

fn serve(tape: &Path, url: &str, body: Value) {
    write_fixture(tape, &get(url), &json_response(&body)).unwrap();
}

fn page(id: &str, edited: &str) -> Value {
    json!({
        "object": "page",
        "id": id,
        "last_edited_time": edited,
        "parent": {"type": "workspace", "workspace": true},
    })
}

fn serve_object(tape: &Path, id: &str, edited: &str) {
    serve(tape, &format!("{BASE}/pages/{id}"), page(id, edited));
}

fn serve_body(tape: &Path, id: &str, markdown: &str, truncated: bool) {
    serve(
        tape,
        &format!("{BASE}/pages/{id}/markdown"),
        json!({"object": "page_markdown", "id": id, "markdown": markdown, "truncated": truncated}),
    );
}

fn serve_comments(tape: &Path, id: &str) {
    serve(
        tape,
        &format!("{BASE}/comments?block_id={id}&page_size={PAGE_SIZE}"),
        json!({"object": "list", "results": [], "has_more": false, "next_cursor": null}),
    );
}

fn serve_page(tape: &Path, id: &str, edited: &str, markdown: &str) {
    serve_object(tape, id, edited);
    serve_body(tape, id, markdown, false);
    serve_comments(tape, id);
}

fn serve_search(tape: &Path, cursor: Option<&str>, results: Value, next: Option<&str>) {
    let mut body = json!({
        "page_size": PAGE_SIZE,
        "sort": { "timestamp": "last_edited_time", "direction": "descending" },
    });
    if let Some(c) = cursor {
        body["start_cursor"] = json!(c);
    }
    let req = HttpRequest::post_json(
        HttpService::Notion,
        format!("{BASE}/search"),
        body.to_string().into_bytes(),
    )
    .header("Accept", "application/json");
    let resp = json!({
        "object": "list",
        "results": results,
        "has_more": next.is_some(),
        "next_cursor": next,
    });
    write_fixture(tape, &req, &json_response(&resp)).unwrap();
}

async fn run(tape: &Path, store: &Path, roots: &[&str]) -> anyhow::Result<FetchSummary> {
    std::env::set_var(PLAYBACK_ENV, tape);
    let db = RawDb::open(store).await.unwrap();
    let summary = fetch(FetchOptions {
        subtree_pages: roots.iter().map(|r| r.to_string()).collect(),
        ..FetchOptions::new(db.clone())
    })
    .await;
    // As the step does: only a run that returns Ok is committed.
    if summary.is_ok() {
        db.commit_all("test").await.unwrap();
    }
    db.close().await;
    summary
}

async fn problems(store: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(store).await.unwrap();
    let rows = sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    rows
}

fn row(key: &str, severity: &str) -> (String, String) {
    (key.to_string(), severity.to_string())
}

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
/// Its signed URL lives only in the response that named it, so the page
/// has to be fetched again for the retry, though it has not moved.
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
async fn comments_that_did_not_list_are_a_warning_on_the_page_until_they_do() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_object(&tape, BRIDGE, EDITED);
    serve_body(&tape, BRIDGE, "Captain's log.\n", false);

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("pages:{BRIDGE}"), "warning")]
    );

    serve_comments(&tape, BRIDGE);
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}

/// A truncated subtree whose follow-up failed left the body incomplete
/// for good, with nothing but a log line to say so.
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
        vec![row(&format!("page_markdown:{HOLODECK}"), "warning")]
    );

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

fn serve_status(tape: &Path, url: &str, status: u16) {
    let resp = HttpResponse {
        status,
        body: format!(r#"{{"object":"error","status":{status}}}"#).into_bytes(),
        ..json_response(&json!({}))
    };
    write_fixture(tape, &get(url), &resp).unwrap();
}

fn unserve(tape: &Path, url: &str) {
    std::fs::remove_file(tape.join("notion").join(fixture_key(&get(url)))).unwrap();
}

async fn pages_to_refetch(store: &Path) -> Vec<String> {
    let db = RawDb::open(store).await.unwrap();
    let mut ids: Vec<String> = db
        .pages_to_refetch(true)
        .await
        .unwrap()
        .into_iter()
        .collect();
    db.close().await;
    ids.sort();
    ids
}

/// A page that failed and was then deleted upstream answered 404 to every
/// later run's retry, and kept its `pages:` row for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_page_gone_upstream_is_retired_not_retried() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let earlier = "2026-08-01T00:00:00.000Z";
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, earlier)]),
        None,
    );
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("pages:{SICKBAY}"), "error")]
    );

    let sickbay = format!("{BASE}/pages/{SICKBAY}");
    serve_status(&tape, &sickbay, 404);
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), None);
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    assert!(pages_to_refetch(&store).await.is_empty());

    // Not asked for again: a request now would miss the tape and fail.
    unserve(&tape, &sickbay);
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}

/// A stored page whose body never comes — Notion answers 404 for the
/// body, or the page is deleted while its body is behind — was fetched
/// again every run, its body forever older than its object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_will_not_come_is_not_fetched_every_run() {
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
    assert_eq!(pages_to_refetch(&store).await, vec![SICKBAY.to_string()]);

    serve_status(&tape, &format!("{BASE}/pages/{SICKBAY}"), 404);
    run(&tape, &store, &[BRIDGE, SICKBAY]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("config:roots:{SICKBAY}"), "warning")]
    );
    assert!(pages_to_refetch(&store).await.is_empty());
    let db = RawDb::open(&store).await.unwrap();
    assert_eq!(
        db.load_pages().await.unwrap().len(),
        2,
        "the ingest deletes nothing"
    );
    db.close().await;
}

async fn stored_pages(store: &Path) -> Vec<String> {
    let db = RawDb::open(store).await.unwrap();
    let mut ids: Vec<String> = db
        .load_pages()
        .await
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    db.close().await;
    ids.sort();
    ids
}

async fn resume_cursor(store: &Path) -> std::collections::HashMap<String, String> {
    let db = RawDb::open(store).await.unwrap();
    let cursor = datalib_etl::doltlite_raw::load_scope_state(db.pool())
        .await
        .unwrap();
    db.close().await;
    cursor
}

/// When the retry guard gave up, the run failed, and a failed run is not
/// committed: every page it had already fetched was thrown away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_guard_that_gives_up_keeps_what_it_fetched() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let earlier = "2026-08-01T00:00:00.000Z";
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_status(&tape, &format!("{BASE}/pages/{SICKBAY}"), 503);
    serve_search(
        &tape,
        None,
        json!([
            page(BRIDGE, EDITED),
            page(SICKBAY, earlier),
            page(HOLODECK, earlier)
        ]),
        None,
    );
    let tick = std::time::Duration::from_millis(1);
    let guard = datalib_etl::retry::RetryGuard::new(
        std::time::Duration::from_secs(3600),
        1,
        tick,
        tick,
        datalib_etl::stop::StopFlag::default(),
    );

    datalib_etl::retry::scope(guard, run(&tape, &store, &[]))
        .await
        .unwrap();
    assert_eq!(stored_pages(&store).await, vec![BRIDGE.to_string()]);
    assert_eq!(
        problems(&store).await,
        vec![row("phase:rate_limit", "error")]
    );
    assert!(resume_cursor(&store).await.is_empty(), "the cursor moved");

    serve_page(&tape, SICKBAY, earlier, "");
    serve_page(&tape, HOLODECK, earlier, "");
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        stored_pages(&store).await,
        vec![
            BRIDGE.to_string(),
            SICKBAY.to_string(),
            HOLODECK.to_string()
        ]
    );
    assert!(problems(&store).await.is_empty());
    assert!(!resume_cursor(&store).await.is_empty());
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
/// so every run fetched every page again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comments_the_credential_may_not_read_are_one_row() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    for id in [BRIDGE, SICKBAY] {
        serve_object(&tape, id, EDITED);
        serve_body(&tape, id, "Captain's log.\n", false);
        serve_status(
            &tape,
            &format!("{BASE}/comments?block_id={id}&page_size={PAGE_SIZE}"),
            403,
        );
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
    assert_eq!(second.official_requests, 2, "only the two page objects");
    assert_eq!(
        problems(&store).await,
        vec![row("listing:comments", "warning")]
    );
}

/// A search that failed past its first page failed the whole step, and
/// would have moved the resume cursor past what it never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_cut_short_keeps_its_pages_and_holds_the_cursor() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_search(&tape, None, json!([page(BRIDGE, EDITED)]), Some("page-2"));

    let summary = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(summary.new_pages, 1);
    assert_eq!(problems(&store).await, vec![row("listing:search", "error")]);
    let db = RawDb::open(&store).await.unwrap();
    let cursor = datalib_etl::doltlite_raw::load_scope_state(db.pool())
        .await
        .unwrap();
    db.close().await;
    assert!(cursor.is_empty(), "the cursor moved: {cursor:?}");

    serve_search(&tape, Some("page-2"), json!([]), None);
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}

/// Search names only what moved since the resume cursor, so a page that
/// failed and has not moved since was never asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_mode_retries_a_page_that_failed() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let earlier = "2026-08-01T00:00:00.000Z";
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, earlier)]),
        None,
    );
    run(&tape, &store, &[]).await.unwrap();
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("pages:{SICKBAY}"), "error")]
    );

    // Upstream: one newer edit, then only what the cursor already covers.
    let later = "2026-09-02T00:00:00.000Z";
    serve_page(&tape, TEN_FORWARD, later, "");
    serve_search(
        &tape,
        None,
        json!([page(TEN_FORWARD, later), page(SICKBAY, earlier)]),
        None,
    );
    serve_page(&tape, SICKBAY, earlier, "");
    run(&tape, &store, &[]).await.unwrap();
    assert!(problems(&store).await.is_empty());
}
