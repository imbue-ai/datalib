//! Widening `only_extract_labels` on an already-synced Gmail mirror.
//!
//! The historyId cursor answers "what changed since last run?", and mail
//! that already sat outside the old label filter never changed — so an
//! incremental run after a widening is a silent no-op unless the run
//! notices the filter moved and walks what is newly in scope. Found on
//! a real account on 2026-09-15: three labels, then no filter, and two
//! further syncs spent 4 quota units each and mirrored nothing.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeSet;

use datalib_etl::http::{HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_email::ingest::{db_path_for, RawDb};
use serde_json::{json, Value};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users";
/// Under `datalib`, the label the mirror started with.
const UNDER_LIB: &str = "18c9f2a1b2c3d601";
/// Under `travel`, admitted by the second config.
const UNDER_TRAVEL: &str = "18c9f2a1b2c3d602";
/// Under neither user label; only an unfiltered walk lists it.
const INBOX_ONLY: &str = "18c9f2a1b2c3d603";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widened_filter_backfills_what_is_newly_in_scope() {
    // One test, three scenarios in sequence: `PLAYBACK_ENV` is
    // process-global, so as separate `#[tokio::test]`s they would race.
    an_unchanged_filter_replays_history_only().await;
    an_added_label_walks_just_that_label().await;
    a_removed_filter_walks_the_whole_account().await;
}

async fn an_unchanged_filter_replays_history_only() {
    let h = Harness::new();
    let first = h.run(&["datalib"]).await;
    assert!(first.full_sync);
    assert_eq!(first.emails_upserted, 1, "{first:?}");

    let second = h.run(&["datalib"]).await;
    assert!(!second.full_sync, "{second:?}");
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert!(second.backfilled_labels.is_empty(), "{second:?}");
    // profile + labels + history.list: the run must not have walked.
    assert_eq!(second.quota_units_spent, 4, "{second:?}");
    assert_eq!(h.mirrored().await, ids(&[UNDER_LIB]));
}

async fn an_added_label_walks_just_that_label() {
    let h = Harness::new();
    h.run(&["datalib"]).await;

    let widened = h.run(&["datalib", "travel"]).await;
    assert!(!widened.full_sync, "the cursor still stands: {widened:?}");
    assert_eq!(widened.backfilled_labels, vec!["travel".to_string()]);
    assert_eq!(widened.emails_upserted, 1, "{widened:?}");
    assert_eq!(h.mirrored().await, ids(&[UNDER_LIB, UNDER_TRAVEL]));

    // The filter is recorded once satisfied, so the next run is quiet.
    let again = h.run(&["datalib", "travel"]).await;
    assert!(again.backfilled_labels.is_empty(), "{again:?}");
    assert_eq!(again.emails_upserted, 0, "{again:?}");
}

async fn a_removed_filter_walks_the_whole_account() {
    let h = Harness::new();
    h.run(&["datalib"]).await;

    let widened = h.run(&[]).await;
    assert!(!widened.full_sync, "the cursor still stands: {widened:?}");
    assert_eq!(widened.backfilled_labels, vec!["*".to_string()]);
    assert_eq!(
        widened.emails_upserted, 2,
        "the whole account was not walked: {widened:?}",
    );
    assert_eq!(
        h.mirrored().await,
        ids(&[UNDER_LIB, UNDER_TRAVEL, INBOX_ONLY])
    );

    let again = h.run(&[]).await;
    assert!(again.backfilled_labels.is_empty(), "{again:?}");
    assert_eq!(again.quota_units_spent, 4, "{again:?}");
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

struct Harness {
    _dir: tempfile::TempDir,
    playback: std::path::PathBuf,
    root: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let playback = dir.path().join("playback");
        let root = dir.path().join("store");
        std::fs::create_dir_all(&root).expect("create store dir");
        write_fixtures(&playback);
        Self {
            _dir: dir,
            playback,
            root,
        }
    }

    async fn run(&self, labels: &[&str]) -> FetchSummary {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        let db = RawDb::open(&db_path_for(&self.root))
            .await
            .expect("open raw db");
        let mut opts = FetchOptions::new(db.clone());
        opts.only_labels = labels.iter().map(|s| s.to_string()).collect();
        let summary = gmail_api::fetch(opts).await;
        db.commit_all("test").await.unwrap();
        db.close().await;
        std::env::remove_var(PLAYBACK_ENV);
        summary.expect("gmail fetch under playback")
    }

    async fn mirrored(&self) -> BTreeSet<String> {
        let db = RawDb::open(&db_path_for(&self.root))
            .await
            .expect("reopen raw db");
        let ids: Vec<String> = sqlx::query_scalar("SELECT gmail_id FROM gmail_messages")
            .fetch_all(db.pool())
            .await
            .expect("read gmail_messages");
        db.commit_all("test").await.unwrap();
        db.close().await;
        ids.into_iter().collect()
    }
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
            { "id": "Label_7", "name": "datalib", "type": "user" },
            { "id": "Label_9", "name": "travel", "type": "user" },
        ]}),
    );
    // Nothing changed since the first run's cursor: the case where an
    // incremental run has nothing to say about mail it never mirrored.
    put(
        &format!("{BASE}/me/history?startHistoryId=1000"),
        &json!({ "historyId": "1000" }),
    );
    put(
        &list_url(Some("Label_7")),
        &json!({ "messages": [{ "id": UNDER_LIB }] }),
    );
    put(
        &list_url(Some("Label_9")),
        &json!({ "messages": [{ "id": UNDER_TRAVEL }] }),
    );
    put(
        &list_url(None),
        &json!({ "messages": [
            { "id": UNDER_LIB }, { "id": UNDER_TRAVEL }, { "id": INBOX_ONLY },
        ]}),
    );
    put(
        &get_url(UNDER_LIB),
        &message(UNDER_LIB, &["INBOX", "Label_7"]),
    );
    put(
        &get_url(UNDER_TRAVEL),
        &message(UNDER_TRAVEL, &["INBOX", "Label_9"]),
    );
    put(&get_url(INBOX_ONLY), &message(INBOX_ONLY, &["INBOX"]));
}

fn list_url(label_id: Option<&str>) -> String {
    let mut url = format!("{BASE}/me/messages?maxResults=500&includeSpamTrash=true");
    if let Some(id) = label_id {
        url.push_str("&labelIds=");
        url.push_str(id);
    }
    url
}

fn get_url(id: &str) -> String {
    format!("{BASE}/me/messages/{id}?format=RAW")
}

fn message(id: &str, label_ids: &[&str]) -> Value {
    let eml = format!(
        "Message-ID: <{id}@example.test>\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
         From: sender@example.test\r\n\
         To: t@example.test\r\n\
         Subject: {id}\r\n\
         \r\n\
         body\r\n",
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
