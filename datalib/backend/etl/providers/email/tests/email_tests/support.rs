//! What the playback tests share: a mirror to download into, the Gmail
//! fixtures they serve, and a progress recorder.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_etl::checkpointer::Cadence;
use datalib_etl::control::DownloadControl;
use datalib_etl::download_metrics::DownloadMetrics;
use datalib_etl::processor::RunCtx;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::raw_store::Sealer;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_email::ingest::{db_path_for, RawDb};
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::{json_response, write_fixture};
use datalib_obs::diagnostics::Diagnostics;
use serde_json::{json, Value};
use tempfile::TempDir;

/// Fixtures go under `playback`, the store under `root`.
pub struct Mirror {
    _dir: TempDir,
    pub playback: PathBuf,
    root: PathBuf,
}

impl Mirror {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("store");
        std::fs::create_dir_all(&root).expect("create store dir");
        Self {
            playback: dir.path().join("playback"),
            root,
            _dir: dir,
        }
    }

    /// Runs one download against the fixtures: the store is opened here,
    /// handed to `download`, committed if the download returned `Ok`, as
    /// the processor does, and closed.
    pub async fn run<T, F>(&self, download: impl FnOnce(RawDb) -> F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let db = RawDb::open(&db_path_for(&self.root))
            .await
            .expect("open raw db");
        let out = playback::scope(&self.playback, download(db.clone())).await;
        if out.is_ok() {
            db.commit_all("test").await.unwrap();
        }
        db.close().await;
        out
    }

    /// Runs one download as the step does, through `RunCtx::run_store`:
    /// it gets a `Sealer`, an `Ok` commits and an `Err` commits nothing.
    /// The cadence is zero, so every point the download calls consistent
    /// is a seal.
    pub async fn run_sealing<T, F>(
        &self,
        download: impl FnOnce(RawDb, Sealer) -> F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let db = RawDb::open(&db_path_for(&self.root))
            .await
            .expect("open raw db");
        let progress = Progress::noop();
        let control = DownloadControl {
            checkpoint_cadence: Some(Cadence {
                at_most_every: Duration::ZERO,
            }),
            ..Default::default()
        };
        let ctx = RunCtx::new(
            "email",
            &self.root,
            "2369-04-14T00:00:00+00:00",
            &progress,
            &control,
            DownloadMetrics::new(),
            Diagnostics::new(),
        );
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        let run = ctx.run_store(pool, Some(cas_pool), |sealer| async move {
            download(db, sealer).await.map(|_| String::new())
        });
        let out = playback::scope(&self.playback, run).await;
        out.map(|_| ())
    }

    /// Opens the store for `read` alone. The file takes one writer at a
    /// time, so nothing else may hold it.
    pub async fn read<T, F>(&self, read: impl FnOnce(RawDb) -> F) -> T
    where
        F: Future<Output = T>,
    {
        let db = RawDb::open(&db_path_for(&self.root))
            .await
            .expect("open raw db");
        let out = read(db.clone()).await;
        db.commit_all("test").await.unwrap();
        db.close().await;
        out
    }

    /// The Gmail ids held: every listed message with a held version.
    pub async fn gmail_ids(&self) -> BTreeSet<String> {
        self.read(|db| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM listed_messages_bookkeeping WHERE held_version IS NOT NULL",
            )
            .fetch_all(db.pool())
            .await
            .expect("read what is held")
        })
        .await
        .into_iter()
        .collect()
    }
}

// ── Gmail fixtures ──────────────────────────────────────────────────

pub const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

pub fn put_gmail(playback: &Path, url: &str, body: &Value) {
    put_gmail_response(playback, url, &json_response(body));
}

pub fn put_gmail_response(playback: &Path, url: &str, response: &HttpResponse) {
    write_fixture(
        playback,
        &HttpRequest::get(HttpService::Gmail, url),
        response,
    )
    .expect("write fixture");
}

/// `t@example.test`'s profile at `history_id`, and its `labels`.
pub fn put_gmail_account(playback: &Path, history_id: &str, labels: Value) {
    put_gmail(
        playback,
        &format!("{GMAIL}/profile"),
        &json!({ "emailAddress": "t@example.test", "historyId": history_id }),
    );
    put_gmail(
        playback,
        &format!("{GMAIL}/labels"),
        &json!({ "labels": labels }),
    );
}

pub fn inbox_label() -> Value {
    json!({ "id": "INBOX", "name": "INBOX", "type": "system" })
}

pub fn gmail_history_url(start: &str) -> String {
    format!("{GMAIL}/history?startHistoryId={start}")
}

/// The enumeration of every message, or of those under `label_ids`.
pub fn gmail_list_url(label_ids: &[&str]) -> String {
    let mut url = format!("{GMAIL}/messages?maxResults=500&includeSpamTrash=true");
    for id in label_ids {
        url.push_str("&labelIds=");
        url.push_str(id);
    }
    url
}

pub fn gmail_get_url(id: &str) -> String {
    format!("{GMAIL}/messages/{id}?format=RAW")
}

/// One `messages.get` answer: a plain message under `label_ids`, its own
/// thread, its `.eml` in Gmail's `raw` encoding.
pub fn gmail_message(id: &str, label_ids: &[&str], subject: &str) -> Value {
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

// ── progress ────────────────────────────────────────────────────────

/// Records what a download announced and what it counted, the way the
/// runner's `RunStoreSink` does. Children share one recorder because the
/// runner relabels every child event back to the step that emitted it.
#[derive(Default, Clone)]
pub struct Recorder {
    lengths: Arc<Mutex<Vec<Option<u64>>>>,
    done: Arc<Mutex<u64>>,
}

impl ProgressSink for Recorder {
    fn set_length(&self, total: Option<u64>) {
        self.lengths.lock().unwrap().push(total);
    }
    fn inc(&self, delta: u64) {
        *self.done.lock().unwrap() += delta;
    }
}

impl Recorder {
    /// Every total the run announced, in order. A `set_length(None)` is
    /// left out: the runner treats it as "no total" and so publishes no
    /// `queued` for it.
    pub fn announcements(&self) -> Vec<u64> {
        self.lengths
            .lock()
            .unwrap()
            .iter()
            .filter_map(|t| *t)
            .collect()
    }

    pub fn final_done(&self) -> u64 {
        *self.done.lock().unwrap()
    }
}
