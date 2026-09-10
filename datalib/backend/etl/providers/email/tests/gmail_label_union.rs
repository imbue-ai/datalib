//! Gmail's `only_extract_labels` means "carrying **any** of these".
//!
//! Driven through the HTTP playback layer, so it runs in CI with no
//! credential and no network. The fixtures model Gmail's real behaviour,
//! measured live on 2026-09-08: repeated `labelIds` in one request
//! **intersect**, so asking for two labels at once returns only the
//! messages carrying both. That is what made a three-label config
//! download nothing at all while reporting success.

use std::collections::BTreeSet;

use datalib_etl::http::{HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_email::ingest::gmail_api::{self, FetchOptions};
use datalib_etl_email::ingest::{db_path_for, RawDb};
use serde_json::{json, Value};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users";

/// Under `datalib` only.
const ONLY_LIB: &str = "18c9f2a1b2c3d401";
/// Under both labels — the one message the old intersecting request
/// would have returned.
const BOTH: &str = "18c9f2a1b2c3d402";
/// Under `travel` only.
const ONLY_TRAVEL: &str = "18c9f2a1b2c3d403";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mirrors_the_union_of_the_configured_labels() {
    let d = tempfile::tempdir().expect("tempdir");
    let playback = d.path().join("playback");
    let root = d.path().join("store");
    std::fs::create_dir_all(&root).expect("create store dir");
    write_fixtures(&playback);

    std::env::set_var(PLAYBACK_ENV, &playback);
    let db = RawDb::open(&db_path_for(&root)).await.expect("open raw db");
    let mut opts = FetchOptions::new(db.clone());
    opts.only_labels = vec!["datalib".to_string(), "travel".to_string()];
    let summary = gmail_api::fetch(opts).await;
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);

    let summary = summary.expect("gmail fetch under playback");
    assert!(
        summary.full_sync,
        "no stored cursor, so this is a full sync"
    );
    assert_eq!(
        summary.emails_upserted, 3,
        "expected the union of both labels, got {summary:?}",
    );
    // The message under both labels is listed by both walks. Fetching it
    // twice would cost 20 quota units and write the same row again.
    assert_eq!(
        summary.blobs_stored, 3,
        "a message under two configured labels was fetched twice: {summary:?}",
    );

    let db = RawDb::open(&db_path_for(&root))
        .await
        .expect("reopen raw db");
    let gmail_ids: Vec<String> = sqlx::query_scalar("SELECT gmail_id FROM gmail_messages")
        .fetch_all(db.pool())
        .await
        .expect("read gmail_messages");
    db.close().await;
    assert_eq!(
        gmail_ids.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ONLY_LIB.to_string(),
            BOTH.to_string(),
            ONLY_TRAVEL.to_string(),
        ]),
        "a message under exactly one of the two labels was dropped — \
         the enumeration intersected the labels instead of unioning them",
    );
}

fn write_fixtures(out: &std::path::Path) {
    let get = |url: &str| HttpRequest::get(HttpService::Gmail, url);
    let put = |url: &str, body: &Value| {
        write_fixture(out, &get(url), &json_response(body)).expect("write fixture")
    };

    put(
        &format!("{BASE}/me/profile"),
        &json!({ "emailAddress": "t@example.test", "historyId": "1000" }),
    );
    put(
        &format!("{BASE}/me/labels"),
        &json!({ "labels": [
            { "id": "INBOX", "name": "INBOX", "type": "system" },
            { "id": "UNREAD", "name": "UNREAD", "type": "system" },
            { "id": "Label_7", "name": "datalib", "type": "user" },
            { "id": "Label_9", "name": "travel", "type": "user" },
        ]}),
    );

    // What Gmail answers when both labels ride on one request: the
    // intersection, not the union. Present so that code combining them
    // fails this test on the row count rather than on a missing fixture.
    put(
        &format!("{}&labelIds=Label_9", list_url("Label_7")),
        &json!({ "messages": [{ "id": BOTH }] }),
    );

    put(
        &list_url("Label_7"),
        &json!({ "messages": [{ "id": ONLY_LIB }, { "id": BOTH }] }),
    );
    put(
        &list_url("Label_9"),
        &json!({ "messages": [{ "id": BOTH }, { "id": ONLY_TRAVEL }] }),
    );

    put(&get_url(ONLY_LIB), &message(ONLY_LIB, &["Label_7"], "lib"));
    put(
        &get_url(BOTH),
        &message(BOTH, &["Label_7", "Label_9"], "both"),
    );
    put(
        &get_url(ONLY_TRAVEL),
        &message(ONLY_TRAVEL, &["Label_9"], "travel"),
    );
}

fn list_url(label_id: &str) -> String {
    format!("{BASE}/me/messages?maxResults=500&includeSpamTrash=true&labelIds={label_id}")
}

fn get_url(id: &str) -> String {
    format!("{BASE}/me/messages/{id}?format=RAW")
}

fn message(id: &str, label_ids: &[&str], subject: &str) -> Value {
    let eml = format!(
        "Message-ID: <{id}@example.test>\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
         From: sender@example.test\r\n\
         To: t@example.test\r\n\
         Subject: {subject}\r\n\
         \r\n\
         body of {subject}\r\n",
    );
    json!({
        "id": id,
        "threadId": id,
        "labelIds": label_ids,
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
