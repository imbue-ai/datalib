//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (`datalib_etl_web::interrupt`).
//! The tape is the TNG account widened until every kind of work has
//! something to do: several days of three metrics, an activity listing
//! of more than one page, details and FIT files, an activity with no
//! file, and wellness days with and without a bundle.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_garmin::ingest::{db_path_for, fetch, FetchOptions, RawDb, ACTIVITY_PAGE};
use datalib_etl_garmin_config::GarminApi;
use datalib_etl_web::http::PLAYBACK_ENV;
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::retry::{self, RetryGuard};
use serde_json::json;

use crate::prune_gate::{Account, PLAYBACK, TODAY};

/// Every table the download fills. The `_bookkeeping` sidecars and
/// `problems` are left out: a run that was cut off has more attempts
/// than one that was not. What the sidecars hold is compared on its
/// own ([`HELD`]): the next run decides what to fetch from it.
const TABLES: &[&str] = &[
    "garmin_account",
    "garmin_devices",
    "garmin_daily",
    "garmin_weigh_ins",
    "garmin_activities",
    "garmin_activity_details",
    "garmin_activity_files",
    "garmin_wellness_files",
    "garmin_items",
    "coverage",
];

/// The tables whose sidecar says what version each row is held at.
const HELD: &[&str] = &[
    "garmin_daily",
    "garmin_activity_details",
    "garmin_activity_files",
    "garmin_wellness_files",
];

async fn dump_held(pool: &sqlx::SqlitePool) -> Result<String> {
    let mut out = String::new();
    for table in HELD {
        out.push_str(&format!("== {table} held\n"));
        // Audited: `table` is a literal of HELD.
        let rows: Vec<(String, bool, Option<String>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT id, fetched_at_utc IS NOT NULL, held_version \
                 FROM {table}_bookkeeping ORDER BY id"
            )))
            .fetch_all(pool)
            .await?;
        for (id, fetched, held) in rows {
            out.push_str(&format!("{id} fetched={fetched} @ {held:?}\n"));
        }
    }
    Ok(out)
}

struct Garmin {
    playback: PathBuf,
    api: GarminApi,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Garmin {
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
                db: db.clone(),
                latchkey: Default::default(),
                api: self.api.clone(),
                today: TODAY,
                progress: Progress::noop(),
                control: DownloadControl {
                    stop,
                    ..Default::default()
                },
                sealer: None,
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
        let pool = datalib_pin::open_reader(&db_path_for(dir)).await?;
        let tables = dump_tables(&pool, TABLES).await;
        let held = dump_held(&pool).await;
        pool.close().await;
        Ok(tables? + &held?)
    }
}

/// The TNG account with enough in it that the activity listing runs to
/// a second page, one activity has no file, and the wellness walk meets
/// both a bundle and a day without one.
pub(crate) fn busy_account() -> Account {
    let mut a = Account::tng();
    a.api.metrics = Some(vec!["daily_summary".into(), "sleep".into(), "hrv".into()]);
    a.api.wellness_files = Some(true);
    let activities = a.spec["activities"].as_array_mut().unwrap();
    for i in 0..ACTIVITY_PAGE {
        let id = 17_010_500_000_i64 + i as i64;
        let start = format!("2369-04-{:02} {:02}:15:00", 2 + i % 12, 6 + i % 9);
        activities.push(json!({
            "listing": {
                "activityId": id,
                "activityName": format!("Phaser drill {i}"),
                "startTimeGMT": start,
                "activityType": {"typeId": 13, "typeKey": "strength_training"},
            },
            "detail": {"activityId": id, "summaryDTO": {"duration": 600.0 + i as f64}},
            "no_file": i == 7,
        }));
    }
    a.spec["wellness"] = json!({
        "2369-04-03": "wellness bundle of 2369-04-03",
        "2369-04-14": "wellness bundle of 2369-04-14",
        "2369-04-15": "wellness bundle of 2369-04-15, so far",
    });
    a.resynthesize();
    a
}

/// The first and last twenty requests, and every seventh between: the
/// account and the first days, the items and the last files, and a walk
/// through each phase in the middle.
fn some(n: u64) -> Vec<u64> {
    (1..=n)
        .filter(|k| *k <= 20 || *k > n.saturating_sub(20) || k % 7 == 0)
        .collect()
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let _serial = PLAYBACK.lock().await;
    let a = busy_account();
    let rig = Garmin {
        playback: a.playback.clone(),
        api: a.api.clone(),
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = a.dir.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, some)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// A store an earlier run filled, and an upstream that has moved since:
/// an activity was renamed, so its detail is owed again; the day's
/// wellness bundle grew. From an empty store nothing is ever *changed*,
/// and a download that remembers "changed" only while it runs passes the
/// test above and fails this one at the changed activity's detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let _serial = PLAYBACK.lock().await;
    let before = busy_account();
    let first = Garmin {
        playback: before.playback.clone(),
        api: before.api.clone(),
        earlier: None,
    };
    let earlier = before.dir.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let mut after = busy_account();
    after.spec["activities"][0]["listing"]["activityName"] =
        "Holodeck run: Dixon Hill, again".into();
    after.spec["activities"][0]["detail"]["activityName"] =
        "Holodeck run: Dixon Hill, again".into();
    after.spec["wellness"]["2369-04-15"] = "wellness bundle of 2369-04-15, by evening".into();
    after.resynthesize();
    let rig = Garmin {
        playback: after.playback.clone(),
        api: after.api.clone(),
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = after.dir.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}
