//! A CalDAV download cut off at any request, then run again, ends with
//! the store an uninterrupted run leaves (`datalib_etl_web::interrupt`).
//! The tape lists one object with its data and one without, so a
//! `multiget` follows the listing; run from an empty store, and from the
//! store that run left against a calendar where an event was edited, one
//! deleted and one added.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::caldav::{self, dav};
use datalib_etl_calendar::ingest::{db_path_for, RawDb};
use datalib_etl_web::http::{HttpMethod, LatchkeySettings};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;

use crate::caldav_playback::{
    account_fixtures, fixture, multistatus, resource, xml, BRIDGE, HOST, RECEPTION, STAFF,
};

/// What the download mirrors and what upstream listed; not the
/// `_bookkeeping` sidecars, whose attempt counts a run that was cut off
/// has more of. What is held for each listed object is read on its own.
pub const TABLES: &[&str] = &[
    "accounts",
    "calendars",
    "ics_objects",
    "dav_resources",
    "dav_unconfirmed",
];

pub async fn contents(db: &RawDb) -> Result<String> {
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

/// Copies the store an earlier run left under `earlier` into `dir`.
pub fn copy_store(earlier: &Path, dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(earlier)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dir.join(entry.file_name()))?;
    }
    Ok(())
}

fn listed_without_data(href: &str, etag: &str) -> String {
    format!(
        "<response><href>{href}</href><propstat><prop><getetag>{etag}</getetag></prop>\
         <status>HTTP/1.1 200 OK</status></propstat></response>"
    )
}

#[derive(Clone, Copy)]
enum Edition {
    Before,
    After,
}

/// The calendar before and after upstream moved: the briefing was
/// edited, the reception deleted, an operations review added.
fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let t = dir.join(match edition {
        Edition::Before => "before",
        Edition::After => "after",
    });
    account_fixtures(&t);
    let bridge = format!("{HOST}{BRIDGE}");
    let (staff, reception, ops) = (
        format!("{BRIDGE}staff.ics"),
        format!("{BRIDGE}reception.ics"),
        format!("{BRIDGE}ops.ics"),
    );
    let sync = |token: &str, body: String| {
        fixture(
            &t,
            HttpMethod::Report,
            &bridge,
            "0",
            &dav::body_sync_collection(token),
            xml(207, &multistatus(&body)),
        )
    };
    let multiget = |hrefs: &[&str], body: String| {
        let hrefs: Vec<String> = hrefs.iter().map(|h| h.to_string()).collect();
        fixture(
            &t,
            HttpMethod::Report,
            &bridge,
            "0",
            &dav::KIND.body_multiget(&hrefs),
            xml(207, &multistatus(&body)),
        )
    };
    match edition {
        Edition::Before => {
            sync(
                "",
                format!(
                    "{}{}<sync-token>data:,100</sync-token>",
                    resource(&staff, "\"s1\"", STAFF),
                    listed_without_data(&reception, "\"r1\""),
                ),
            );
            multiget(&[&reception], resource(&reception, "\"r1\"", RECEPTION));
            sync(
                "data:,100",
                "<sync-token>data:,100</sync-token>".to_string(),
            );
        }
        Edition::After => {
            let staff_v2 = STAFF.replace("Senior staff briefing", "Senior staff briefing (Deck 8)");
            let ops_ics = RECEPTION
                .replace("tng-reception", "tng-ops")
                .replace("Reception for the Klingon delegation", "Operations review");
            sync(
                "data:,100",
                format!(
                    "{}{}<response><href>{reception}</href>\
                     <status>HTTP/1.1 404 Not Found</status></response>\
                     <sync-token>data:,101</sync-token>",
                    resource(&staff, "\"s2\"", &staff_v2),
                    listed_without_data(&ops, "\"o1\""),
                ),
            );
            multiget(&[&ops], resource(&ops, "\"o1\"", &ops_ics));
            sync(
                "data:,101",
                "<sync-token>data:,101</sync-token>".to_string(),
            );
        }
    }
    t
}

struct Caldav {
    playback: PathBuf,
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Caldav {
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
        let download = caldav::fetch(caldav::FetchOptions {
            db: db.clone(),
            server_url: format!("{HOST}/"),
            calendars: Vec::new(),
            window: None,
            latchkey: LatchkeySettings::default(),
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
        let out = contents(&db).await;
        db.close().await;
        out
    }
}

pub fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Caldav {
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
    let first = Caldav {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Caldav {
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
