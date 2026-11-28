//! A Baby Connect download cut off at any request, then run again, ends
//! with the store an uninterrupted run leaves (`datalib_etl_web::interrupt`):
//! from an empty store, and from the store that run left against an
//! account two days on, with an entry edited.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_baby_connect::ingest::{self, db_path_for, RawDb};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;

use crate::tape::{self, Edition};

const TABLES: &[&str] = &["kids", "users", "entries", "coverage"];

struct BabyConnect {
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
impl Rig for BabyConnect {
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
            now: Some(self.edition.now()),
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            ..ingest::FetchOptions::new(db.clone())
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
        let out = dump_tables(db.pool(), TABLES).await?;
        db.close().await;
        Ok(out)
    }
}

async fn all_cuts(rig: &BabyConnect, scratch: &Path) {
    for how in [How::Kill, How::Stop] {
        let scratch = scratch.join(format!("cuts-{how:?}"));
        every_cut_resumes(rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

fn tape_of(root: &Path, edition: Edition) -> PathBuf {
    let dir = root.join(format!("{edition:?}"));
    tape::write(&dir, edition);
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = BabyConnect {
        playback: tape_of(d.path(), Edition::Before),
        edition: Edition::Before,
        earlier: None,
    };
    all_cuts(&rig, d.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = BabyConnect {
        playback: tape_of(d.path(), Edition::Before),
        edition: Edition::Before,
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = BabyConnect {
        playback: tape_of(d.path(), Edition::After),
        edition: Edition::After,
        earlier: Some(earlier),
    };
    all_cuts(&rig, &d.path().join("later")).await;
}
