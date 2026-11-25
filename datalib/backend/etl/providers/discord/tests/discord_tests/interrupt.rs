//! A Discord download cut off at any request, then run again, ends with
//! the store an uninterrupted run leaves (`datalib_etl_web::interrupt`):
//! from an empty store, and from the store that run left against an
//! account that moved on.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_discord::ingest::{self, db_path_for, RawDb};
use datalib_etl_discord_config::DiscordSync;
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;
use datalib_time::IsoOffsetTimestamp;

use crate::playback::{later, options, sync, TABLES};
use crate::world::{self, World};

struct Discord {
    playback: PathBuf,
    now: IsoOffsetTimestamp,
    /// A window that takes in every message re-reads whatever a cut lost,
    /// and so hides a claim written too early; `0` hides nothing.
    refresh_window_days: i64,
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
impl Rig for Discord {
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
        let sync = DiscordSync {
            refresh_window_days: Some(self.refresh_window_days),
            ..sync()
        };
        let download = ingest::fetch(ingest::FetchOptions {
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            ..options(db.clone(), sync, self.now.clone())
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

async fn all_cuts(rig: &Discord, scratch: &Path) {
    for how in [How::Kill, How::Stop] {
        let scratch = scratch.join(format!("cuts-{how:?}"));
        every_cut_resumes(rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Discord {
        playback: World::before().tape(d.path(), "before"),
        now: world::now(),
        refresh_window_days: 0,
        earlier: None,
    };
    all_cuts(&rig, d.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Discord {
        playback: World::before().tape(d.path(), "before"),
        now: world::now(),
        refresh_window_days: 0,
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    for refresh_window_days in [0, 60] {
        let rig = Discord {
            playback: World::after().tape(d.path(), "after"),
            now: later(),
            refresh_window_days,
            earlier: Some(earlier.clone()),
        };
        let scratch = d.path().join(format!("window-{refresh_window_days}"));
        all_cuts(&rig, &scratch).await;
    }
}
