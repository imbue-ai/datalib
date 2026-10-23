//! chatgpt.com's conversation listing leaves archived conversations out,
//! and a complete walk deletes what it does not name. So archiving a
//! chat deleted it from the mirror. The walk lists the archived ones too.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_chatgpt::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_chatgpt::synthesize::ChatgptSynth;
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::playback;
use datalib_etl_web::retry::{self, RetryGuard};
use datalib_etl_web::synthesize::{write_fixture, Synthesizer};
use serde_json::{json, Value};
use tempfile::TempDir;

/// Archiving a conversation after it was mirrored keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_conversation_stays_in_the_mirror() {
    let acct = Account::new();
    acct.list(&["c-enterprise", "c-stargazer"], &[]);
    acct.run().await.unwrap();

    acct.list(&["c-enterprise"], &["c-stargazer"]);
    let second = acct.run().await.unwrap();
    assert_eq!(second.pruned, 0, "{second:?}");
    assert_eq!(acct.stored().await, ["c-enterprise", "c-stargazer"]);
}

/// A conversation archived before the first sync is mirrored like any
/// other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conversation_archived_before_the_first_sync_is_mirrored() {
    let acct = Account::new();
    acct.list(&["c-enterprise"], &["c-stargazer"]);
    acct.run().await.unwrap();
    assert_eq!(acct.stored().await, ["c-enterprise", "c-stargazer"]);
}

/// A conversation neither listing names is still deleted: listing the
/// archived ones did not turn the prune off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conversation_in_neither_listing_is_deleted() {
    let acct = Account::new();
    acct.list(&["c-enterprise", "c-stargazer"], &[]);
    acct.run().await.unwrap();

    acct.list(&["c-enterprise"], &[]);
    let second = acct.run().await.unwrap();
    assert_eq!(second.pruned, 1, "{second:?}");
    assert_eq!(acct.stored().await, ["c-enterprise"]);
}

/// The plain listing alone is not the whole account, so when the
/// archived one fails nothing is deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_archived_listing_deletes_nothing() {
    let acct = Account::new();
    acct.list(&["c-enterprise", "c-stargazer"], &[]);
    acct.run().await.unwrap();

    acct.list(&["c-enterprise"], &[]);
    acct.refuse(&archived_page(0));
    let second = acct.run().await.unwrap();
    assert_eq!(second.pruned, 0, "{second:?}");
    assert_eq!(acct.stored().await, ["c-enterprise", "c-stargazer"]);
}

const BASE: &str = "https://chatgpt.com";

fn archived_page(offset: usize) -> HttpRequest {
    HttpRequest::get(
        HttpService::Chatgpt,
        format!("{BASE}/backend-api/conversations?offset={offset}&limit=100&order=updated&is_archived=true"),
    )
    .header("Accept", "application/json")
}

fn write_json(path: &Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

/// One account's input snapshot, its playback tape, and the store a run
/// writes.
struct Account {
    _dir: TempDir,
    api: PathBuf,
    playback: PathBuf,
    raw: PathBuf,
}

impl Account {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let a = Account {
            api: dir.path().join("api"),
            playback: dir.path().join("playback"),
            raw: dir.path().join("raw"),
            _dir: dir,
        };
        fs::create_dir_all(&a.raw).unwrap();
        write_json(&a.api.join("me.json"), &json!({"id": "u-1"}));
        a
    }

    /// The account's conversations now, and its archived ones.
    fn list(&self, active: &[&str], archived: &[&str]) {
        for (file, ids) in [
            ("conversations.json", active),
            ("conversations_archived.json", archived),
        ] {
            let items: Vec<Value> = ids
                .iter()
                .map(|id| json!({"id": id, "update_time": 1.0, "title": id}))
                .collect();
            write_json(&self.api.join(file), &Value::Array(items));
            for id in ids {
                write_json(
                    &self.api.join(format!("conversations/{id}.json")),
                    &json!({"id": id, "update_time": 1.0, "mapping": {}, "title": id}),
                );
            }
        }
        ChatgptSynth::new(&self.api)
            .synthesize(&self.playback)
            .unwrap();
    }

    fn refuse(&self, req: &HttpRequest) {
        let resp = HttpResponse {
            status: 500,
            headers: Default::default(),
            body: b"{\"detail\":\"no\"}".to_vec(),
            duration_ms: 0,
        };
        write_fixture(&self.playback, req, &resp).unwrap();
    }

    async fn run(&self) -> anyhow::Result<FetchSummary> {
        let db = RawDb::open(&db_path_for(&self.raw)).await.unwrap();
        let control = datalib_etl::control::DownloadControl::default();
        // One failure is the give-up, so a refused page ends at once.
        let fast = Duration::from_millis(1);
        let guard = RetryGuard::new(
            Duration::from_secs(3600),
            1,
            fast,
            fast,
            control.stop.clone(),
        );
        let download = retry::scope(
            guard,
            fetch(FetchOptions {
                control,
                ..FetchOptions::new(db.clone())
            }),
        );
        let s = playback::scope(&self.playback, download).await;
        if s.is_ok() {
            db.commit_all("test").await.unwrap();
        }
        db.close().await;
        s
    }

    async fn stored(&self) -> Vec<String> {
        let db = RawDb::open(&db_path_for(&self.raw)).await.unwrap();
        let ids = sqlx::query_scalar("SELECT id FROM conversations ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
        db.close().await;
        ids
    }
}
