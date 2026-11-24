//! A Google Contacts download cut off at any request, then run again,
//! ends with the store an uninterrupted run leaves
//! (`datalib_etl_web::interrupt`). The listing takes two pages and two
//! photos follow; run from an empty store, and from the store that run
//! left against contacts where one was edited with a new photo, one
//! deleted and one added.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_contacts::ingest::google::connections_url;
use datalib_etl_contacts::ingest::{db_path_for, RawDb};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use serde_json::json;

use crate::google_playback::{
    account, deleted, download, fixture, group, groups, link, page, person, photo, ME,
};

const TABLES: &[&str] = &[
    "accounts",
    "addressbooks",
    "google_people",
    "google_contact_groups",
    "google_photos",
];

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
    account(&t, ME);
    match edition {
        Edition::Before => {
            groups(&t, json!([group("bridge", "Bridge crew", true)]));
            fixture(
                &t,
                &connections_url(None, None),
                page(
                    vec![
                        person("c1", "William Riker", Some(&link("riker-1")), &["bridge"]),
                        person("c2", "Data", None, &["bridge"]),
                    ],
                    Some("p2"),
                    None,
                ),
            );
            fixture(
                &t,
                &connections_url(None, Some("p2")),
                page(
                    vec![person("c3", "Tasha Yar", Some(&link("yar-1")), &["bridge"])],
                    None,
                    Some("s1"),
                ),
            );
            fixture(
                &t,
                &connections_url(Some("s1"), None),
                page(vec![], None, Some("s1")),
            );
            photo(&t, &link("riker-1"), 200, "image/png");
            photo(&t, &link("yar-1"), 200, "image/png");
        }
        Edition::After => {
            groups(&t, json!([group("away", "Away team", true)]));
            fixture(
                &t,
                &connections_url(Some("s1"), None),
                page(
                    vec![
                        person("c1", "William T. Riker", Some(&link("riker-2")), &["away"]),
                        deleted("c3"),
                    ],
                    Some("p2"),
                    None,
                ),
            );
            fixture(
                &t,
                &connections_url(Some("s1"), Some("p2")),
                page(
                    vec![person("c5", "Worf", Some(&link("worf-1")), &["away"])],
                    None,
                    Some("s2"),
                ),
            );
            fixture(
                &t,
                &connections_url(Some("s2"), None),
                page(vec![], None, Some("s2")),
            );
            photo(&t, &link("riker-2"), 200, "image/png");
            photo(&t, &link("worf-1"), 200, "image/png");
        }
    }
    t
}

struct Google {
    playback: PathBuf,
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Google {
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
        let control = DownloadControl {
            stop,
            ..Default::default()
        };
        download(db, &self.playback, control).await.map(|_| ())
    }

    async fn seal(&self, db: RawDb) -> Result<()> {
        db.commit_all("test").await?;
        db.close().await;
        Ok(())
    }

    async fn contents(&self, dir: &Path) -> Result<String> {
        let db = self.open(dir).await?;
        let out = dump_tables(db.pool(), TABLES).await;
        db.close().await;
        out
    }
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

async fn resumes_from_every_cut(rig: &Google, scratch: &Path) {
    for how in [How::Kill, How::Stop] {
        let scratch = scratch.join(format!("cuts-{how:?}"));
        every_cut_resumes(rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_google_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Google {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    resumes_from_every_cut(&rig, d.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_google_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Google {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Google {
        playback: tape(d.path(), Edition::After),
        earlier: Some(earlier),
    };
    resumes_from_every_cut(&rig, d.path()).await;
}
