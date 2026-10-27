//! A Calendly download cut off at any request, then run again, ends
//! with the store an uninterrupted run leaves
//! (`datalib_etl_web::interrupt`). The meetings list over two pages and
//! one meeting's invitees over two more; run from an empty store, and
//! from the store that run left against a Calendly where one meeting was
//! renamed, one is gone and one was booked.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::calendly::{self, events_url, invitees_url};
use datalib_etl_calendar::ingest::{db_path_for, RawDb};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::playback;
use serde_json::json;

use crate::caldav_interrupt::{copy_store, every};
use crate::calendly_playback::{event, fixture, invitee, me, page, USER};

/// What the download mirrors; not the `_bookkeeping` sidecars, whose
/// attempt counts a run that was cut off has more of. What is held for
/// each meeting's invitees is read on its own.
const TABLES: &[&str] = &[
    "accounts",
    "calendars",
    "calendly_events",
    "calendly_invitees",
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
    me(&t);
    let counseling = event(
        "EVCOUNSEL0000001",
        "Counseling session",
        "2026-11-02T17:00:00.000000Z",
        "active",
    );
    let poker = |name: &str| {
        event(
            "EVPOKER00000001",
            name,
            "2026-11-05T17:00:00.000000Z",
            "active",
        )
    };
    let recital = event(
        "EVRECITAL000001",
        "Data's violin recital",
        "2026-11-09T17:00:00.000000Z",
        "active",
    );
    let riker = page(
        json!([invitee(
            "EVPOKER00000001",
            "INRIKER0000001",
            "William Riker",
            "riker@enterprise.test"
        )]),
        None,
    );
    match edition {
        Edition::Before => {
            fixture(
                &t,
                &events_url(USER, None, None),
                page(
                    json!([counseling, poker("Officers' poker night")]),
                    Some("p2"),
                ),
            );
            fixture(
                &t,
                &events_url(USER, None, Some("p2")),
                page(json!([recital]), None),
            );
            fixture(
                &t,
                &invitees_url("EVCOUNSEL0000001", None),
                page(
                    json!([invitee(
                        "EVCOUNSEL0000001",
                        "INBARCLAY000001",
                        "Reginald Barclay",
                        "barclay@enterprise.test"
                    )]),
                    Some("i2"),
                ),
            );
            fixture(
                &t,
                &invitees_url("EVCOUNSEL0000001", Some("i2")),
                page(
                    json!([invitee(
                        "EVCOUNSEL0000001",
                        "INTROI00000001",
                        "Deanna Troi",
                        "troi@enterprise.test"
                    )]),
                    None,
                ),
            );
            fixture(&t, &invitees_url("EVPOKER00000001", None), riker);
            fixture(
                &t,
                &invitees_url("EVRECITAL000001", None),
                page(
                    json!([invitee(
                        "EVRECITAL000001",
                        "INDATA00000001",
                        "Data",
                        "data@enterprise.test"
                    )]),
                    None,
                ),
            );
        }
        Edition::After => {
            let briefing = event(
                "EVBRIEF00000001",
                "Away team briefing",
                "2026-11-12T17:00:00.000000Z",
                "active",
            );
            fixture(
                &t,
                &events_url(USER, None, None),
                page(
                    json!([counseling, poker("Officers' poker night (high stakes)")]),
                    Some("p2"),
                ),
            );
            fixture(
                &t,
                &events_url(USER, None, Some("p2")),
                page(json!([briefing]), None),
            );
            fixture(&t, &invitees_url("EVPOKER00000001", None), riker);
            fixture(
                &t,
                &invitees_url("EVBRIEF00000001", None),
                page(
                    json!([invitee(
                        "EVBRIEF00000001",
                        "INWORF00000001",
                        "Worf",
                        "worf@enterprise.test"
                    )]),
                    None,
                ),
            );
        }
    }
    t
}

struct Calendly {
    playback: PathBuf,
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Calendly {
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
        let download = calendly::fetch(calendly::FetchOptions {
            db: db.clone(),
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

async fn contents(db: &RawDb) -> Result<String> {
    let mut out = dump_tables(db.pool(), TABLES).await?;
    out.push_str("== held\n");
    let held: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version \
         FROM calendly_invitees_bookkeeping ORDER BY id",
    )
    .fetch_all(db.pool())
    .await?;
    for (id, fetched, version) in held {
        out.push_str(&format!("{id} fetched={fetched} @ {version:?}\n"));
    }
    Ok(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_calendly_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let rig = Calendly {
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
async fn a_later_calendly_sync_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let first = Calendly {
        playback: tape(d.path(), Edition::Before),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let rig = Calendly {
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
