//! A Todoist download cut off at any request, then run again, ends with
//! the store an uninterrupted run leaves (`datalib_etl_web::interrupt`):
//! from an empty store, and from the store that run left against an
//! account that moved on.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_todoist::ingest::{self, db_path_for, RawDb};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;

use crate::playback::{options, TABLES};
use crate::tape::{self, Edition};

struct Todoist {
    playback: PathBuf,
    edition: Edition,
    earlier: Option<PathBuf>,
}

fn copy_store(earlier: &Path, dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(earlier)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dir.join(entry.file_name()))?;
    }
    Ok(())
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[async_trait]
impl Rig for Todoist {
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
        let download = ingest::fetch(ingest::FetchOptions {
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            ..options(db.clone(), self.edition)
        });
        playback::scope(&self.playback, download).await.map(|_| ())
    }

    async fn seal(&self, db: RawDb) -> Result<()> {
        db.commit_all("test").await?;
        db.close().await;
        Ok(())
    }

    async fn contents(&self, dir: &Path) -> Result<String> {
        let db = self.open(dir).await?;
        let mut out = dump_tables(db.pool(), TABLES).await?;
        let held: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, held_version FROM completed_items_bookkeeping ORDER BY id")
                .fetch_all(db.pool())
                .await?;
        for (id, version) in held {
            out.push_str(&format!("{id} @ {version:?}\n"));
        }
        db.close().await;
        Ok(out)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Todoist {
        playback: tape::tape(d.path(), Edition::Before),
        edition: Edition::Before,
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// From an empty store the token is always `*` and nothing is ever
/// changed; a download that stored the token ahead of what it covers
/// passes the test above and fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Todoist {
        playback: tape::tape(d.path(), Edition::Before),
        edition: Edition::Before,
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Todoist {
        playback: tape::tape(d.path(), Edition::After),
        edition: Edition::After,
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
