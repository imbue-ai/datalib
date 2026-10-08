//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (docs/dev/plans/sync_state.md §8).
//! The tape is a small TNG account: three conversations, one with two
//! attachments, one of which chatgpt.com no longer serves. Run from an
//! empty store and from the store an earlier run left, against an
//! account that has moved since: one conversation edited and given a
//! new file, one new, one deleted.
//!
//! The cut the rig cannot make, between a conversation's row and the
//! attachment edges that name its files, is made by taking the blob
//! store away ([`a_death_between_the_row_and_its_attachments`]).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_chatgpt::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_chatgpt::synthesize::ChatgptSynth;
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::synthesize::{json_response, write_fixture, Synthesizer};
use serde_json::{json, Value};

const BASE: &str = "https://chatgpt.com";
const FILES: &str = "https://files.example.test";

/// Every table the download fills, with the blob store's beside them.
/// The `_bookkeeping` sidecars and `problems` are left out: a run that
/// was cut off has more attempts than one that was not. What the
/// sidecars hold is compared on its own ([`HELD`]).
const TABLES: &[&str] = &["me", "conversations", "chatgpt_attachments"];
const HELD: &[&str] = &["conversations", "chatgpt_attachments"];

async fn dump_held(pool: &sqlx::SqlitePool) -> Result<String> {
    let mut out = String::new();
    for table in HELD {
        out.push_str(&format!("== {table} held\n"));
        // Audited: `table` is a literal of HELD.
        let rows: Vec<(String, bool, Option<String>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT id, fetched_at_utc IS NOT NULL, held_version \
                 FROM {table}_bookkeeping ORDER BY id"
            )))
            .fetch_all(pool)
            .await?;
        for (id, fetched, held) in rows {
            out.push_str(&format!("{id} fetched={fetched} @ {held:?}\n"));
        }
    }
    Ok(out)
}

fn iso(epoch: f64) -> String {
    chrono::DateTime::from_timestamp_micros((epoch * 1_000_000.0).round() as i64)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string()
}

fn with_files(id: &str, update_time: f64, files: &[&str]) -> Value {
    let attachments: Vec<Value> = files
        .iter()
        .map(|f| json!({"id": f, "name": format!("{f}.txt"), "mime_type": "text/plain"}))
        .collect();
    json!({
        "id": id,
        "update_time": update_time,
        "title": id,
        "mapping": {"n1": {"message": {"metadata": {"attachments": attachments}}}},
    })
}

fn write_json(path: &Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn metadata(tape: &Path, file: &str) {
    let req = HttpRequest::get(
        HttpService::Chatgpt,
        format!("{BASE}/backend-api/files/{file}/download"),
    )
    .header("Accept", "application/json");
    write_fixture(
        tape,
        &req,
        &json_response(&json!({"download_url": format!("{FILES}/{file}?sig=abc")})),
    )
    .unwrap();
    write_fixture(
        tape,
        &HttpRequest::get(HttpService::Chatgpt, format!("{FILES}/{file}?sig=abc")),
        &HttpResponse {
            status: 200,
            headers: [("content-type".to_string(), "text/plain".to_string())].into(),
            body: format!("the bytes of {file}").into_bytes(),
            duration_ms: 0,
        },
    )
    .unwrap();
}

fn gone(tape: &Path, file: &str) {
    let req = HttpRequest::get(
        HttpService::Chatgpt,
        format!("{BASE}/backend-api/files/{file}/download"),
    )
    .header("Accept", "application/json");
    write_fixture(
        tape,
        &req,
        &HttpResponse {
            status: 404,
            headers: Default::default(),
            body: b"{\"detail\":\"no\"}".to_vec(),
            duration_ms: 0,
        },
    )
    .unwrap();
}

/// The account before and after upstream moved.
#[derive(Clone, Copy)]
enum Edition {
    Before,
    After,
}

fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let name = match edition {
        Edition::Before => "before",
        Edition::After => "after",
    };
    let api = dir.join(format!("api-{name}"));
    let t = dir.join(format!("tape-{name}"));
    write_json(
        &api.join("me.json"),
        &json!({"id": "u-1", "email": "x@y.test"}),
    );
    let convs: Vec<Value> = match edition {
        Edition::Before => vec![
            with_files("c-picard", 3.0, &["f-log", "f-lost"]),
            with_files("c-riker", 2.0, &[]),
            with_files("c-data", 1.0, &[]),
        ],
        Edition::After => vec![
            with_files("c-troi", 5.0, &[]),
            with_files("c-picard", 4.0, &["f-log", "f-lost", "f-warp"]),
            with_files("c-riker", 2.0, &[]),
        ],
    };
    let listing: Vec<Value> = convs
        .iter()
        .map(|c| {
            json!({
                "id": c["id"],
                "update_time": iso(c["update_time"].as_f64().unwrap()),
                "title": c["title"],
            })
        })
        .collect();
    write_json(&api.join("conversations.json"), &Value::Array(listing));
    for c in &convs {
        write_json(
            &api.join(format!("conversations/{}.json", c["id"].as_str().unwrap())),
            c,
        );
    }
    ChatgptSynth::new(&api).synthesize(&t).unwrap();
    metadata(&t, "f-log");
    gone(&t, "f-lost");
    if matches!(edition, Edition::After) {
        metadata(&t, "f-warp");
    }
    t
}

struct Chatgpt {
    playback: PathBuf,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Chatgpt {
    type Store = RawDb;

    async fn seed(&self, dir: &Path) -> Result<()> {
        if let Some(earlier) = &self.earlier {
            for entry in fs::read_dir(earlier)? {
                let entry = entry?;
                fs::copy(entry.path(), dir.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        fetch(FetchOptions {
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            ..FetchOptions::new(db.clone())
        })
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
        let out = async {
            let tables = dump_tables(db.pool(), TABLES).await?;
            let blobs = dump_tables(db.cas().pool(), &["cas_objects"]).await?;
            let held = dump_held(db.pool()).await?;
            Ok::<_, anyhow::Error>(tables + &blobs + &held)
        }
        .await;
        db.close().await;
        out
    }
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Chatgpt {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// A store an earlier run filled, and an upstream that has moved since.
/// From an empty store nothing is ever *changed*, and a download that
/// remembers "changed" only while it runs passes the test above and
/// fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Chatgpt {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Chatgpt {
        playback: tape(d.path(), Edition::After),
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// Bug C2 of the plan: the conversation's row and its freshness were
/// committed in one transaction and its attachment edges in a later one.
/// A run that died between them left a conversation that read as up to
/// date with no edge to say it had files, and no later run fetched
/// them. The rig cuts only at requests, so the gap is opened here by
/// taking the blob store away: the edge flush fails, whatever the run
/// had committed before it is sealed, and the next run has to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_between_the_row_and_its_attachments_leaves_them_owed() {
    let d = tempfile::tempdir().unwrap();
    let rig = Chatgpt {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let whole = d.path().join("whole");
    fs::create_dir_all(&whole).unwrap();
    let db = rig.open(&whole).await.unwrap();
    rig.download(&db, StopFlag::new()).await.unwrap();
    rig.seal(db).await.unwrap();
    let want = rig.contents(&whole).await.unwrap();

    let cut = d.path().join("cut");
    fs::create_dir_all(&cut).unwrap();
    let db = rig.open(&cut).await.unwrap();
    db.cas().pool().close().await;
    let err = rig
        .download(&db, StopFlag::new())
        .await
        .expect_err("the blob store is gone, so the run cannot finish");
    assert!(format!("{err:#}").contains("closed"), "{err:#}");
    rig.seal(db).await.unwrap();

    let db = rig.open(&cut).await.unwrap();
    rig.download(&db, StopFlag::new()).await.unwrap();
    rig.seal(db).await.unwrap();
    let got = rig.contents(&cut).await.unwrap();
    assert_eq!(
        got, want,
        "the run after the death did not finish the store"
    );
}
