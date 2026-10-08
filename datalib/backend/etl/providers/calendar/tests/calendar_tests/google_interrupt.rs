//! A Google Calendar download cut off at any request, then run again,
//! ends with the store an uninterrupted run leaves
//! (docs/dev/plans/sync_state.md §8). The primary calendar lists over
//! two pages; run from an empty store, and from the store that run left
//! against calendars where a series was cancelled, an event edited, one
//! added and the away team's one event cancelled.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::google::{self, events_url};
use datalib_etl_calendar::ingest::{db_path_for, RawDb};
use datalib_etl_web::http::{LatchkeySettings, PLAYBACK_ENV};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use serde_json::json;

use crate::caldav_interrupt::{copy_store, every};
use crate::google_playback::{calendar_list, fixture, page, AWAY, PRIMARY};

/// The listing carries whole events and the token moves with the last
/// page, so the data tables and the token are the whole state.
const TABLES: &[&str] = &["accounts", "calendars", "google_events"];

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
    calendar_list(&t);
    let series = json!({"id": "staff01", "status": "confirmed", "summary": "Senior staff briefing",
        "start": {"dateTime": "2026-01-05T09:00:00-08:00"}, "end": {"dateTime": "2026-01-05T10:00:00-08:00"},
        "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,TH"]});
    let moved = json!({"id": "staff01_20260312T170000Z", "status": "confirmed", "recurringEventId": "staff01",
        "originalStartTime": {"dateTime": "2026-03-12T09:00:00-08:00"},
        "start": {"dateTime": "2026-03-12T11:00:00-07:00"}, "end": {"dateTime": "2026-03-12T12:30:00-07:00"}});
    let reception = |summary: &str| {
        json!({"id": "reception01", "status": "confirmed", "summary": summary,
            "start": {"dateTime": "2026-09-18T19:00:00-07:00"}, "end": {"dateTime": "2026-09-18T22:00:00-07:00"}})
    };
    let away = json!({"id": "away01", "status": "confirmed", "summary": "Away mission: Rigel VII",
        "start": {"dateTime": "2026-10-01T08:00:00-07:00"}, "end": {"dateTime": "2026-10-01T18:00:00-07:00"}});
    match edition {
        Edition::Before => {
            fixture(
                &t,
                &events_url(PRIMARY, None, None),
                page(json!([series, moved]), Some("p2"), None),
            );
            fixture(
                &t,
                &events_url(PRIMARY, None, Some("p2")),
                page(
                    json!([reception("Reception for the Klingon delegation")]),
                    None,
                    Some("s1"),
                ),
            );
            fixture(
                &t,
                &events_url(AWAY, None, None),
                page(json!([away]), None, Some("a1")),
            );
            fixture(
                &t,
                &events_url(PRIMARY, Some("s1"), None),
                page(json!([]), None, Some("s1")),
            );
            fixture(
                &t,
                &events_url(AWAY, Some("a1"), None),
                page(json!([]), None, Some("a1")),
            );
        }
        Edition::After => {
            let ops = json!({"id": "ops01", "status": "confirmed", "summary": "Operations review",
                "start": {"dateTime": "2026-10-02T09:00:00-07:00"}, "end": {"dateTime": "2026-10-02T10:00:00-07:00"}});
            fixture(
                &t,
                &events_url(PRIMARY, Some("s1"), None),
                page(
                    json!([
                        {"id": "staff01", "status": "cancelled"},
                        reception("Reception for the Klingon delegation (Ten Forward)"),
                        ops
                    ]),
                    None,
                    Some("s2"),
                ),
            );
            fixture(
                &t,
                &events_url(PRIMARY, Some("s2"), None),
                page(json!([]), None, Some("s2")),
            );
            fixture(
                &t,
                &events_url(AWAY, Some("a1"), None),
                page(
                    json!([{"id": "away01", "status": "cancelled"}]),
                    None,
                    Some("a2"),
                ),
            );
            fixture(
                &t,
                &events_url(AWAY, Some("a2"), None),
                page(json!([]), None, Some("a2")),
            );
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
        match &self.earlier {
            Some(earlier) => copy_store(earlier, dir),
            None => Ok(()),
        }
    }

    async fn open(&self, dir: &Path) -> Result<RawDb> {
        RawDb::open(&db_path_for(dir)).await
    }

    async fn download(&self, db: &RawDb, stop: StopFlag) -> Result<()> {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        google::fetch(google::FetchOptions {
            db: db.clone(),
            calendars: Vec::new(),
            window: None,
            latchkey: LatchkeySettings::default(),
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
        let out = dump_tables(db.pool(), TABLES).await;
        db.close().await;
        out
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_google_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Google {
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
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
