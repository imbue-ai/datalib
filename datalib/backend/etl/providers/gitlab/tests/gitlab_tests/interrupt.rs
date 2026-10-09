//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (`datalib_etl_web::interrupt`).
//! The tape is a small TNG project: two MRs, one with a discussion. Run
//! from an empty store and from the store an earlier run left, against
//! a project that has moved since: an MR edited and discussed again,
//! and a new one.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_forge_ingest_common::Bounds;
use datalib_etl_gitlab::ingest::{
    db_path_for, fetch, search_url, FetchOptions, RawDb, DEFAULT_SCOPES,
};
use datalib_etl_web::http::PLAYBACK_ENV;
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::retry::{self, RetryGuard};
use serde_json::{json, Value};

use crate::support::*;

/// Every table the download fills. The `_bookkeeping` sidecars and
/// `problems` are left out: a run that was cut off has more attempts
/// than one that was not. What the MR sidecar holds is compared on its
/// own: the next run decides what to fetch from it.
const TABLES: &[&str] = &[
    "self_identity",
    "merge_requests",
    "discussions",
    "listed_change_requests",
    "coverage",
];

async fn dump_held(pool: &sqlx::SqlitePool) -> Result<String> {
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version \
         FROM merge_requests_bookkeeping ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = String::from("== merge_requests held\n");
    for (id, fetched, held) in rows {
        out.push_str(&format!("{id} fetched={fetched} @ {held:?}\n"));
    }
    Ok(out)
}

const T1: &str = "2369-04-10T00:00:00.000Z";
const T2: &str = "2369-04-12T00:00:00.000Z";
const T4: &str = "2369-04-15T10:00:00.000Z";
const T5: &str = "2369-04-15T11:00:00.000Z";
/// The top of what a run at the TNG clock covers, as the listing takes
/// it: where the next run's listing starts.
const SINCE: &str = "2369-04-15T00:00:00.000Z";

/// The project before and after upstream moved: MR 1 was edited and
/// discussed again, and MR 3 appeared.
#[derive(Clone, Copy)]
enum Edition {
    Before,
    After,
}

fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let t = dir.join(match edition {
        Edition::Before => "before",
        Edition::After => "after",
    });
    serve_user(&t);
    let (mr1_at, mr1_title) = match edition {
        Edition::Before => (T2, "Safety protocols"),
        Edition::After => (T4, "Safety protocols, revised"),
    };
    let mut mrs: Vec<(u64, &str, Value, Vec<Value>)> = vec![
        (
            1,
            mr1_at,
            mr(1, mr1_at, mr1_title),
            vec![discussion("d1", "Looks good.", T2)],
        ),
        (2, T1, mr(2, T1, "Warp core alignment"), Vec::new()),
    ];
    if matches!(edition, Edition::After) {
        mrs[0].3.push(discussion("d2", "Make it so.", T4));
        mrs.push((3, T5, mr(3, T5, "Poker night"), Vec::new()));
    }
    for (iid, _, detail, discussions) in &mrs {
        serve(&t, &mr_url(*iid), detail.clone());
        serve(&t, &discussions_url(*iid), json!(discussions));
    }
    let all: Vec<Value> = mrs.iter().map(|(iid, at, ..)| item(*iid, at)).collect();
    // What moved since the earlier run's listing: nothing before, MR 1
    // and MR 3 after.
    let moved: Vec<Value> = mrs
        .iter()
        .filter(|(_, at, ..)| *at > SINCE)
        .map(|(iid, at, ..)| item(*iid, at))
        .collect();
    let resumed = Bounds {
        lo: Some(SINCE.to_string()),
        hi: None,
    };
    for scope in DEFAULT_SCOPES {
        serve(
            &t,
            &search_url(scope, USER_ID, &Bounds::default()),
            json!(all),
        );
        serve(&t, &search_url(scope, USER_ID, &resumed), json!(moved));
    }
    t
}

struct Gitlab {
    playback: PathBuf,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Gitlab {
    type Store = RawDb;

    async fn seed(&self, dir: &Path) -> Result<()> {
        if let Some(earlier) = &self.earlier {
            for entry in std::fs::read_dir(earlier)? {
                let entry = entry?;
                std::fs::copy(entry.path(), dir.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        let fast = std::time::Duration::from_millis(1);
        let guard = RetryGuard::new(
            std::time::Duration::from_secs(3600),
            100,
            fast,
            fast,
            stop.clone(),
        );
        retry::scope(
            guard,
            fetch(FetchOptions {
                refresh_window_days: 0,
                control: DownloadControl {
                    stop,
                    ..Default::default()
                },
                ..FetchOptions::new(db.clone(), crate::tng_now())
            }),
        )
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
            let held = dump_held(db.pool()).await?;
            Ok::<_, anyhow::Error>(tables + &held)
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
    let rig = Gitlab {
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
    let first = Gitlab {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Gitlab {
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
