//! A Gmail download cut off at any request, then run again, ends with
//! the store an uninterrupted run leaves (`datalib_etl_web::interrupt`).
//! Twice: a first sync, whose `messages.list` takes three pages;
//! and a later sync from the store the first left, against an upstream
//! where a message arrived in an existing thread, one gained a label,
//! one lost a label and one was deleted, over two `history.list` pages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_email::ingest::gmail_api::{fetch, FetchOptions};
use datalib_etl_email::ingest::{db_path_for, RawDb};
use datalib_etl_web::http::HttpResponse;
use datalib_etl_web::interrupt::{every_cut_resumes, How, Rig};
use datalib_etl_web::playback;
use serde_json::{json, Value};

use crate::jmap_interrupt::{contents, copy_store};
use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, put_gmail_response,
};

struct Gmail {
    playback: PathBuf,
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Gmail {
    type Store = RawDb;

    async fn seed(&self, dir: &Path) -> Result<()> {
        match &self.earlier {
            Some(earlier) => copy_store(earlier, dir),
            None => Ok(()),
        }
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        let mut opts = FetchOptions::new(db.clone());
        // Two messages to a write, so a cut lands between writes.
        opts.flush_batch = Some(2);
        opts.control = DownloadControl {
            stop,
            ..Default::default()
        };
        playback::scope(&self.playback, fetch(opts))
            .await
            .map(|_| ())
    }

    async fn seal(&self, db: RawDb) -> Result<()> {
        db.commit_all("test").await?;
        db.close().await;
        Ok(())
    }

    async fn contents(&self, dir: &Path) -> Result<String> {
        let db = self.open(dir).await?;
        let out = contents(&db).await;
        db.close().await;
        out
    }
}

fn id(n: u8) -> String {
    format!("18c9f2a1b2c3d6{n:02x}")
}

const AWAY_TEAM: &str = "Label_7";

fn labels() -> Value {
    json!([inbox_label(), { "id": AWAY_TEAM, "name": "away team", "type": "user" }])
}

/// One message under `label_ids`; the first two and the ninth share a
/// thread.
fn message(n: u8, label_ids: &[&str]) -> Value {
    let mut m = gmail_message(&id(n), label_ids, &format!("Captain's log {n}"));
    if n == 2 || n == 9 {
        m["threadId"] = json!(id(1));
    }
    m
}

/// `messages.list` over `ids`, three to a page.
fn record_listing(playback: &Path, ids: &[String]) {
    let pages: Vec<&[String]> = ids.chunks(3).collect();
    for (i, page) in pages.iter().enumerate() {
        let mut url = gmail_list_url(&[]);
        if i > 0 {
            url.push_str(&format!("&pageToken=p{i}"));
        }
        let mut body = json!({
            "messages": page.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            "resultSizeEstimate": ids.len(),
        });
        if i + 1 < pages.len() {
            body["nextPageToken"] = json!(format!("p{}", i + 1));
        }
        put_gmail(playback, &url, &body);
    }
}

/// Eight messages in the inbox, the second and third on the away team.
fn record_before(playback: &Path) {
    put_gmail_account(playback, "9001", labels());
    record_listing(playback, &(1..=8).map(id).collect::<Vec<_>>());
    for n in 1..=8 {
        let on: &[&str] = if n == 2 || n == 3 {
            &["INBOX", AWAY_TEAM]
        } else {
            &["INBOX"]
        };
        put_gmail(playback, &gmail_get_url(&id(n)), &message(n, on));
    }
    put_gmail(
        playback,
        &gmail_history_url("9001"),
        &json!({ "historyId": "9001" }),
    );
}

/// [`record_before`], later: the ninth message arrived, the fourth
/// joined the away team, the second left it, the fifth was deleted.
fn record_after(playback: &Path) {
    record_before(playback);
    put_gmail_account(playback, "9002", labels());
    let named = |n: u8| json!({ "message": { "id": id(n) } });
    put_gmail(
        playback,
        &gmail_history_url("9001"),
        &json!({
            "history": [
                { "messagesAdded": [named(9)] },
                { "labelsAdded": [named(4)] },
            ],
            "nextPageToken": "h2",
            "historyId": "9002",
        }),
    );
    put_gmail(
        playback,
        &format!("{}&pageToken=h2", gmail_history_url("9001")),
        &json!({
            "history": [
                { "messagesDeleted": [named(5)] },
                { "labelsRemoved": [named(2)] },
            ],
            "historyId": "9002",
        }),
    );
    put_gmail(
        playback,
        &gmail_history_url("9002"),
        &json!({ "historyId": "9002" }),
    );
    record_listing(playback, &[1, 2, 3, 4, 6, 7, 8, 9].map(id));
    put_gmail(playback, &gmail_get_url(&id(9)), &message(9, &["INBOX"]));
    put_gmail(
        playback,
        &gmail_get_url(&id(4)),
        &message(4, &["INBOX", AWAY_TEAM]),
    );
    put_gmail(playback, &gmail_get_url(&id(2)), &message(2, &["INBOX"]));
    put_gmail_response(
        playback,
        &gmail_get_url(&id(5)),
        &HttpResponse {
            status: 404,
            headers: BTreeMap::new(),
            body: b"{}".to_vec(),
            duration_ms: 0,
        },
    );
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Gmail {
        playback: d.path().join("playback"),
        earlier: None,
    };
    record_before(&rig.playback);
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// From an empty store nothing is ever relabeled or deleted, so a
/// download that skips what it already holds passes the test above and
/// fails this one at the first relabeled message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Gmail {
        playback: d.path().join("playback-before"),
        earlier: None,
    };
    record_before(&first.playback);
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Gmail {
        playback: d.path().join("playback-after"),
        earlier: Some(earlier),
    };
    record_after(&rig.playback);
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
