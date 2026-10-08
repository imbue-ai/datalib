//! What the playback tests share: a tape of TNG merge requests served
//! request by request, one download into a store, and the store read
//! back.

use std::path::Path;

use datalib_etl_gitlab::ingest::{BASE, PER_PAGE};
use datalib_etl_web::http::{HttpRequest, HttpService};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};

pub const PROJECT: &str = "starfleet/enterprise";
/// [`PROJECT`] as a path segment of the API.
pub const PID: &str = "starfleet%2Fenterprise";
pub const USER_ID: i64 = 17010001;

pub fn serve(tape: &Path, url: &str, body: Value) {
    write_fixture(
        tape,
        &HttpRequest::get(HttpService::Gitlab, url),
        &json_response(&body),
    )
    .unwrap();
}

pub fn serve_user(tape: &Path) {
    serve(
        tape,
        &format!("{BASE}/user"),
        json!({"id": USER_ID, "username": "jlpicard", "web_url": "https://gitlab.com/jlpicard"}),
    );
}

pub fn mr_url(iid: u64) -> String {
    format!("{BASE}/projects/{PID}/merge_requests/{iid}")
}

pub fn discussions_url(iid: u64) -> String {
    format!("{BASE}/projects/{PID}/merge_requests/{iid}/discussions?per_page={PER_PAGE}")
}

pub fn mr(iid: u64, updated_at: &str, title: &str) -> Value {
    json!({
        "iid": iid,
        "title": title,
        "web_url": format!("https://gitlab.com/{PROJECT}/-/merge_requests/{iid}"),
        "state": "opened",
        "updated_at": updated_at,
        "source_branch": "feat",
        "target_branch": "main",
    })
}

/// A listing's entry for an MR: what the download reads of it.
pub fn item(iid: u64, updated_at: &str) -> Value {
    json!({
        "iid": iid,
        "web_url": format!("https://gitlab.com/{PROJECT}/-/merge_requests/{iid}"),
        "updated_at": updated_at,
    })
}

pub fn discussion(id: &str, note: &str, at: &str) -> Value {
    json!({
        "id": id,
        "individual_note": false,
        "notes": [{"id": 1, "body": note, "updated_at": at, "author": {"username": "wtriker"}}],
    })
}
