//! A Google Drive download cut off at any request, then run again, ends
//! with the store an uninterrupted run leaves
//! (`datalib_etl_web::interrupt`): from an empty store, and from the
//! store that run left against an account where the Doc moved on.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_google_drive::ingest::{self, db_path_for, RawDb};
use datalib_etl_google_drive_config::GoogleDriveApi;
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;

use crate::playback::TABLES;
use crate::tape::{self, Edition};

struct Drive {
    playback: PathBuf,
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
impl Rig for Drive {
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
            db: db.clone(),
            latchkey: LatchkeySettings::default(),
            api: GoogleDriveApi::default(),
            progress: Default::default(),
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            sealer: None,
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
        let held: Vec<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT id, fetched_at_utc IS NOT NULL, held_version \
             FROM files_bookkeeping ORDER BY id",
        )
        .fetch_all(db.pool())
        .await?;
        for (id, fetched, version) in held {
            out.push_str(&format!("{id} fetched={fetched} @ {version:?}\n"));
        }
        db.close().await;
        Ok(out)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Drive {
        playback: tape::tape(d.path(), Edition::Before),
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// From an empty store nothing is ever *changed*; a download that read
/// a stored file as done whatever its version passes the test above and
/// fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Drive {
        playback: tape::tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Drive {
        playback: tape::tape(d.path(), Edition::After),
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
