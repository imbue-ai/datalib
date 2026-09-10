//! A message the enumeration named but `messages.get` would not return.
//!
//! Two outcomes that look identical at the call site and are not. A 404
//! means the message was deleted between the list and the get: there is
//! nothing to come back for, so the run is complete and the cursor may
//! advance. Any other failure means the message still exists and we
//! still want it — and because storing the cursor makes the next run
//! incremental, `history.list` would only name what *changed*, so a
//! message that merely failed to fetch would never be named again.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeMap;

use datalib_etl::http::{HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_email::download::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_email::download::{db_path_for, RawDb};
use serde_json::{json, Value};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users";
const GOOD: &str = "18c9f2a1b2c3d501";
const BAD: &str = "18c9f2a1b2c3d502";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_holds_the_cursor_and_a_deletion_does_not() {
    // One test, two scenarios in sequence: `PLAYBACK_ENV` is
    // process-global, so as separate `#[tokio::test]`s they would race
    // and clear each other's fixture root mid-request.
    a_500_holds_the_cursor().await;
    a_404_is_a_deletion_and_lets_the_cursor_advance().await;
}

/// The regression this file exists for. Google returns 500s, and 500 is
/// not in the transport's retryable set, so it reaches the fetch loop on
/// the first attempt.
async fn a_500_holds_the_cursor() {
    let (summary, cursor) = run_with_bad_status(500).await;

    assert_eq!(summary.emails_upserted, 1, "the good message still lands");
    assert_eq!(
        summary.messages_failed, 1,
        "the failure must be counted, not swallowed: {summary:?}",
    );
    assert_eq!(
        cursor, None,
        "the cursor advanced past a message this run never fetched — the \
         next run goes incremental and will never name it again",
    );
}

/// The other half, and the reason holding the cursor cannot simply be
/// unconditional: a deleted message is not work left undone.
async fn a_404_is_a_deletion_and_lets_the_cursor_advance() {
    let (summary, cursor) = run_with_bad_status(404).await;

    assert_eq!(summary.emails_upserted, 1);
    assert_eq!(
        summary.messages_failed, 0,
        "a deletion is not a failure: {summary:?}",
    );
    assert_eq!(
        cursor.as_deref(),
        Some("9001"),
        "nothing was left undone, so the run may record where it got to",
    );
}

/// Mirrors one good message and one that answers `bad_status`. Returns
/// the summary and the stored `historyId` cursor, if any.
async fn run_with_bad_status(bad_status: u16) -> (FetchSummary, Option<String>) {
    let d = tempfile::tempdir().expect("tempdir");
    let playback = d.path().join("playback");
    let root = d.path().join("store");
    std::fs::create_dir_all(&root).expect("create store dir");
    write_fixtures(&playback, bad_status);

    std::env::set_var(PLAYBACK_ENV, &playback);
    let db = RawDb::open(&db_path_for(&root)).await.expect("open raw db");
    let summary = gmail_api::fetch(FetchOptions::new(db.clone())).await;
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);
    let summary = summary.expect("gmail fetch under playback");

    // Closed above, reopened here: a second live pool on one store makes
    // each other's `dolt_commit` fail.
    let db = RawDb::open(&db_path_for(&root))
        .await
        .expect("reopen raw db");
    let cursor: Option<String> =
        sqlx::query_scalar("SELECT last_seen_at FROM sync_scope_state WHERE scope = ?")
            .bind("gmail:t@example.test:historyId")
            .fetch_optional(db.pool())
            .await
            .expect("read the cursor");
    db.close().await;
    (summary, cursor)
}

fn write_fixtures(out: &std::path::Path, bad_status: u16) {
    let get = |url: &str| HttpRequest::get(HttpService::Gmail, url);
    let put = |url: &str, body: &Value| {
        write_fixture(out, &get(url), &json_response(body)).expect("write fixture")
    };

    put(
        &format!("{BASE}/me/profile"),
        &json!({ "emailAddress": "t@example.test", "historyId": "9001" }),
    );
    put(
        &format!("{BASE}/me/labels"),
        &json!({ "labels": [{ "id": "INBOX", "name": "INBOX", "type": "system" }] }),
    );
    put(
        &format!("{BASE}/me/messages?maxResults=500&includeSpamTrash=true"),
        &json!({ "messages": [{ "id": GOOD }, { "id": BAD }] }),
    );
    put(&get_url(GOOD), &message(GOOD));

    let refused = HttpResponse {
        status: bad_status,
        headers: BTreeMap::new(),
        body: b"{\"error\":{\"message\":\"nope\"}}".to_vec(),
        duration_ms: 0,
    };
    write_fixture(out, &get(&get_url(BAD)), &refused).expect("write fixture");
}

fn get_url(id: &str) -> String {
    format!("{BASE}/me/messages/{id}?format=RAW")
}

fn message(id: &str) -> Value {
    let eml = format!(
        "Message-ID: <{id}@example.test>\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
         From: sender@example.test\r\n\
         To: t@example.test\r\n\
         Subject: kept\r\n\
         \r\n\
         body\r\n",
    );
    json!({
        "id": id,
        "threadId": id,
        "labelIds": ["INBOX"],
        "internalDate": "1788000000000",
        "raw": base64url(eml.as_bytes()),
    })
}

/// Gmail's `raw` alphabet: RFC 4648 §5, unpadded.
fn base64url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}
