//! What the playback tests share: a tape of TNG pages served request
//! by request, one download into a store, and the store read back.

use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_notion::ingest::official::{BASE, PAGE_SIZE};
use datalib_etl_notion::ingest::{fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_web::http::{fixture_key, HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};

pub const BRIDGE: &str = "1701d000-0000-4000-8000-000000000001";
pub const SICKBAY: &str = "1701d000-0000-4000-8000-000000000002";
pub const HOLODECK: &str = "1701d000-0000-4000-8000-000000000003";
pub const TEN_FORWARD: &str = "1701d000-0000-4000-8000-000000000004";
pub const EDITED: &str = "2026-09-01T00:00:00.000Z";

pub fn get(url: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Notion, url).header("Accept", "application/json")
}

pub fn serve(tape: &Path, url: &str, body: Value) {
    write_fixture(tape, &get(url), &json_response(&body)).unwrap();
}

pub fn page(id: &str, edited: &str) -> Value {
    json!({
        "object": "page",
        "id": id,
        "last_edited_time": edited,
        "parent": {"type": "workspace", "workspace": true},
    })
}

pub fn serve_object(tape: &Path, id: &str, edited: &str) {
    serve(tape, &format!("{BASE}/pages/{id}"), page(id, edited));
}

pub fn serve_body(tape: &Path, id: &str, markdown: &str, truncated: bool) {
    serve(
        tape,
        &format!("{BASE}/pages/{id}/markdown"),
        json!({"object": "page_markdown", "id": id, "markdown": markdown, "truncated": truncated}),
    );
}

pub fn comments_url(id: &str) -> String {
    format!("{BASE}/comments?block_id={id}&page_size={PAGE_SIZE}")
}

pub fn serve_comments(tape: &Path, id: &str) {
    serve_comment_list(tape, id, json!([]));
}

pub fn serve_comment_list(tape: &Path, id: &str, results: Value) {
    serve(
        tape,
        &comments_url(id),
        json!({"object": "list", "results": results, "has_more": false, "next_cursor": null}),
    );
}

/// One comment on `page_id`, hanging off the page itself or off `block`.
pub fn comment(id: &str, page_id: &str, block: Option<&str>) -> Value {
    let parent = match block {
        Some(b) => json!({"type": "block_id", "block_id": b}),
        None => json!({"type": "page_id", "page_id": page_id}),
    };
    json!({
        "object": "comment",
        "id": id,
        "discussion_id": format!("d-{id}"),
        "parent": parent,
        "created_time": EDITED,
        "last_edited_time": EDITED,
        "rich_text": [{"plain_text": "Make it so."}],
    })
}

pub fn serve_page(tape: &Path, id: &str, edited: &str, markdown: &str) {
    serve_object(tape, id, edited);
    serve_body(tape, id, markdown, false);
    serve_comments(tape, id);
}

pub fn serve_search(tape: &Path, cursor: Option<&str>, results: Value, next: Option<&str>) {
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

pub fn serve_status(tape: &Path, url: &str, status: u16) {
    let resp = HttpResponse {
        status,
        body: format!(r#"{{"object":"error","status":{status}}}"#).into_bytes(),
        ..json_response(&json!({}))
    };
    write_fixture(tape, &get(url), &resp).unwrap();
}

pub fn unserve(tape: &Path, url: &str) {
    std::fs::remove_file(tape.join("notion").join(fixture_key(&get(url)))).unwrap();
}

/// One download into `store` against `tape`, as the step runs it:
/// committed only when it returns `Ok`.
pub async fn run(tape: &Path, store: &Path, roots: &[&str]) -> anyhow::Result<FetchSummary> {
    run_with(tape, store, |o| FetchOptions {
        subtree_pages: roots.iter().map(|r| r.to_string()).collect(),
        ..o
    })
    .await
}

pub async fn run_with(
    tape: &Path,
    store: &Path,
    adjust: impl FnOnce(FetchOptions) -> FetchOptions,
) -> anyhow::Result<FetchSummary> {
    std::env::set_var(PLAYBACK_ENV, tape);
    let db = RawDb::open(store).await.unwrap();
    let summary = fetch(adjust(FetchOptions::new(db.clone()))).await;
    if summary.is_ok() {
        db.commit_all("test").await.unwrap();
    }
    db.close().await;
    summary
}

pub async fn problems(store: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(store).await.unwrap();
    let rows = sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    rows
}

pub fn row(key: &str, severity: &str) -> (String, String) {
    (key.to_string(), severity.to_string())
}

pub async fn stored_pages(store: &Path) -> Vec<String> {
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

/// `(page_id, comment_id)` for every stored comment.
pub async fn stored_comments(store: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(store).await.unwrap();
    let mut out: Vec<(String, String)> = db
        .load_comments()
        .await
        .unwrap()
        .into_iter()
        .map(|(c, page)| {
            (
                page.unwrap_or_default(),
                c["id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    db.close().await;
    out.sort();
    out
}
