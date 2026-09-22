//! The Manage screen's "N queued" chip is the DAG runner subtracting a
//! step's increments from the last total that step announced. A download
//! that never announces one gets a bare count instead — no remaining, no
//! countdown — which is what Gmail and Fastmail showed while Slack, which
//! does announce one, counted down.
//!
//! So the assertion that matters is not "a total was announced" but
//! "the last total announced equals what the run counted" — a bar that
//! announces a stale or partial total reads as stuck just as badly as
//! one that announces none. Both tests have been watched failing: with
//! the announcement removed the series ends at 0 of 3, and with the
//! skipped-id tick removed a re-walk ends at 0 of 3 the other way.

use std::sync::{Arc, Mutex};

use datalib_etl::http::{HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_email::ingest::gmail_api::{self, FetchOptions};
use datalib_etl_email::ingest::{db_path_for, RawDb};
use serde_json::{json, Value};

/// Records what a download announced and what it counted, the way the
/// runner's `RunStoreSink` does. Children share one recorder because the
/// runner relabels every child event back to the step that emitted it.
#[derive(Default)]
pub(crate) struct Recorder {
    lengths: Arc<Mutex<Vec<Option<u64>>>>,
    done: Arc<Mutex<u64>>,
}

impl Clone for Recorder {
    fn clone(&self) -> Self {
        Self {
            lengths: self.lengths.clone(),
            done: self.done.clone(),
        }
    }
}

impl ProgressSink for Recorder {
    fn set_length(&self, total: Option<u64>) {
        self.lengths.lock().unwrap().push(total);
    }
    fn inc(&self, delta: u64) {
        *self.done.lock().unwrap() += delta;
    }
    fn child(&self, _prefix: &str) -> Arc<dyn ProgressSink> {
        Arc::new(self.clone())
    }
}

impl Recorder {
    /// Every total the run announced, in order. `-1` stands for a
    /// `set_length(None)`, which the runner treats as "no total" and so
    /// publishes no `queued` for.
    pub(crate) fn announcements(&self) -> Vec<u64> {
        self.lengths
            .lock()
            .unwrap()
            .iter()
            .filter_map(|t| *t)
            .collect()
    }

    pub(crate) fn final_done(&self) -> u64 {
        *self.done.lock().unwrap()
    }
}

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users";
const IDS: [&str; 3] = ["18c9f2a1b2c3d401", "18c9f2a1b2c3d402", "18c9f2a1b2c3d403"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gmail_walk_announces_a_total_so_the_chip_can_count_down() {
    let d = tempfile::tempdir().expect("tempdir");
    let playback = d.path().join("playback");
    let root = d.path().join("store");
    std::fs::create_dir_all(&root).expect("create store dir");
    write_fixtures(&playback);

    let recorder = Recorder::default();
    std::env::set_var(PLAYBACK_ENV, &playback);
    let db = RawDb::open(&db_path_for(&root)).await.expect("open raw db");
    let mut opts = FetchOptions::new(db.clone());
    opts.progress = Progress::new(Arc::new(recorder.clone()));
    let summary = gmail_api::fetch(opts).await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);

    let summary = summary.expect("gmail fetch under playback");
    assert_eq!(summary.emails_upserted, IDS.len());

    let announced = recorder.announcements();
    let last = *announced.last().expect("a total was announced");
    assert_eq!(
        last,
        IDS.len() as u64,
        "the last total announced was {last}, not the {} messages the \
         walk listed; the chip would not reach zero (series: {announced:?})",
        IDS.len(),
    );
    assert_eq!(
        recorder.final_done(),
        IDS.len() as u64,
        "every listed id must tick the bar, or it stalls short of its total",
    );
}

/// A re-walk of a mailbox already mirrored fetches nothing: every id is
/// skipped before `messages.get`. The bar must still reach its total,
/// or a routine incremental run leaves the chip pinned near full.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_that_fetches_nothing_still_reaches_zero_queued() {
    let d = tempfile::tempdir().expect("tempdir");
    let playback = d.path().join("playback");
    let root = d.path().join("store");
    std::fs::create_dir_all(&root).expect("create store dir");
    write_fixtures(&playback);
    std::env::set_var(PLAYBACK_ENV, &playback);

    // First run mirrors everything.
    let db = RawDb::open(&db_path_for(&root)).await.expect("open raw db");
    gmail_api::fetch(FetchOptions::new(db.clone()))
        .await
        .expect("first gmail fetch");
    db.commit_all("test").await.unwrap();
    db.close().await;

    // Second run: `full_resync`, so it walks the same ids again and
    // skips every one of them as already held.
    let recorder = Recorder::default();
    let db = RawDb::open(&db_path_for(&root))
        .await
        .expect("reopen raw db");
    let mut opts = FetchOptions::new(db.clone());
    opts.config.full_resync = true;
    opts.progress = Progress::new(Arc::new(recorder.clone()));
    let summary = gmail_api::fetch(opts).await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);

    let summary = summary.expect("second gmail fetch under playback");
    assert_eq!(
        summary.messages_already_had,
        IDS.len(),
        "the second run should have skipped every id: {summary:?}",
    );
    let announced = recorder.announcements();
    let last = *announced.last().expect("a total was announced");
    assert_eq!(
        recorder.final_done(),
        last,
        "the bar stopped at {} of {last}: a skipped id never ticked it, \
         so \"N queued\" stays pinned near full for the whole run",
        recorder.final_done(),
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
        &json!({ "labels": [{ "id": "INBOX", "name": "INBOX", "type": "system" }]}),
    );
    // One unrestricted walk, one page. `resultSizeEstimate` is what the
    // bar counts down from; Gmail sends it on every page.
    put(
        &format!("{BASE}/me/messages?maxResults=500&includeSpamTrash=true"),
        &json!({
            "messages": IDS.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            "resultSizeEstimate": IDS.len(),
        }),
    );
    for id in IDS {
        put(&format!("{BASE}/me/messages/{id}?format=RAW"), &message(id));
    }
}

fn message(id: &str) -> Value {
    let eml = format!(
        "Message-ID: <{id}@example.test>\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
         From: sender@example.test\r\n\
         To: t@example.test\r\n\
         Subject: {id}\r\n\
         \r\n\
         body of {id}\r\n",
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
