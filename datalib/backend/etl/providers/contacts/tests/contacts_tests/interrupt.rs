//! A CardDAV download cut off at any request, then run again, ends with
//! the store an uninterrupted run leaves (docs/dev/plans/sync_state.md
//! §8). The tape lists two cards with their data and two without, so a
//! `multiget` follows the listing; run from an empty store, and from the
//! store that run left against an address book where a card was edited,
//! one deleted and one added.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_contacts::ingest::{self, api, db_path_for, RawDb};
use datalib_etl_web::http::{HttpMethod, LatchkeySettings, PLAYBACK_ENV};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};

use crate::carddav_playback::{
    account_fixtures, card, cards, fixture, multistatus, resource, xml, BOOK, BRIDGE_V1, BRIDGE_V2,
    HOST,
};

/// What the download mirrors and what upstream listed; not the
/// `_bookkeeping` sidecars, whose attempt counts a run that was cut off
/// has more of. What is held for each listed object is read on its own.
const TABLES: &[&str] = &[
    "accounts",
    "addressbooks",
    "contacts",
    "contact_group_members",
    "contact_categories",
    "dav_resources",
    "dav_unconfirmed",
];

async fn contents(db: &RawDb) -> Result<String> {
    let mut out = dump_tables(db.pool(), TABLES).await?;
    out.push_str("== held\n");
    let held: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version \
         FROM dav_resources_bookkeeping ORDER BY id",
    )
    .fetch_all(db.pool())
    .await?;
    for (id, fetched, version) in held {
        out.push_str(&format!("{id} fetched={fetched} @ {version:?}\n"));
    }
    Ok(out)
}

/// A card named with its etag only, for the `multiget` to fetch.
fn listed_without_data(uid: &str, etag: &str) -> String {
    format!(
        "<response><href>{BOOK}{uid}.vcf</href><propstat><prop><getetag>{etag}</getetag></prop>\
         <status>HTTP/1.1 200 OK</status></propstat></response>"
    )
}

#[derive(Clone, Copy)]
enum Edition {
    Before,
    After,
}

/// The address book before and after upstream moved: Picard's card was
/// edited, Data's deleted, Worf's added.
fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let t = dir.join(match edition {
        Edition::Before => "before",
        Edition::After => "after",
    });
    account_fixtures(&t);
    let book = format!("{HOST}{BOOK}");
    let (v1, v2) = (cards(BRIDGE_V1), cards(BRIDGE_V2));
    let sync = |token: &str, body: String| {
        fixture(
            &t,
            HttpMethod::Report,
            &book,
            "0",
            &api::body_sync_collection(token),
            xml(207, &multistatus(&body)),
        )
    };
    let multiget = |uids: &[&str], body: String| {
        let hrefs: Vec<String> = uids.iter().map(|u| format!("{BOOK}{u}.vcf")).collect();
        fixture(
            &t,
            HttpMethod::Report,
            &book,
            "0",
            &api::KIND.body_multiget(&hrefs),
            xml(207, &multistatus(&body)),
        )
    };
    match edition {
        Edition::Before => {
            sync(
                "",
                format!(
                    "{}{}{}{}<sync-token>data:,1</sync-token>",
                    resource("tng-picard", "\"p1\"", card(&v1, "tng-picard")),
                    resource("tng-riker", "\"r1\"", card(&v1, "tng-riker")),
                    listed_without_data("tng-data", "\"d1\""),
                    listed_without_data("tng-senior-staff", "\"s1\""),
                ),
            );
            multiget(
                &["tng-data", "tng-senior-staff"],
                format!(
                    "{}{}",
                    resource("tng-data", "\"d1\"", card(&v1, "tng-data")),
                    resource("tng-senior-staff", "\"s1\"", card(&v1, "tng-senior-staff")),
                ),
            );
            sync("data:,1", "<sync-token>data:,1</sync-token>".to_string());
        }
        Edition::After => {
            sync(
                "data:,1",
                format!(
                    "{}{}<response><href>{BOOK}tng-data.vcf</href>\
                     <status>HTTP/1.1 404 Not Found</status></response>\
                     <sync-token>data:,2</sync-token>",
                    resource("tng-picard", "\"p2\"", card(&v2, "tng-picard")),
                    listed_without_data("tng-worf", "\"w1\""),
                ),
            );
            multiget(
                &["tng-worf"],
                resource("tng-worf", "\"w1\"", card(&v2, "tng-worf")),
            );
            sync("data:,2", "<sync-token>data:,2</sync-token>".to_string());
        }
    }
    t
}

struct Carddav {
    playback: PathBuf,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Carddav {
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
        ingest::fetch(ingest::FetchOptions {
            latchkey: LatchkeySettings::default(),
            db: db.clone(),
            server_url: format!("{HOST}/"),
            addressbooks: vec!["Bridge".to_string()],
            progress: Default::default(),
            control: DownloadControl {
                stop,
                ..Default::default()
            },
            sealer: None,
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
        let out = contents(&db).await;
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
    let rig = Carddav {
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

/// From an empty store nothing is ever *changed*; a download that reads
/// a stored row as "done" passes the test above and fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Carddav {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Carddav {
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
