//! A download cut off at any request, then run again, ends with the
//! store an uninterrupted run leaves (docs/dev/plans/sync_state.md §8).
//! The tape is two meters reporting every six hours over three weeks,
//! and then, for the run from an earlier store, ten more days plus one
//! reading that arrived late, inside the overlap the next run re-asks.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;
use datalib_etl_web::http::{HttpResponse, PLAYBACK_ENV};
use datalib_etl_web::interrupt::{dump_tables, every_cut_resumes, How, Rig};
use datalib_etl_web::retry::{self, RetryGuard};
use datalib_etl_web::synthesize::write_fixture;
use datalib_etl_yolink::ingest::{
    db_path_for, fetch, requested, window_request, windows_of, FetchOptions, RawDb,
};
use datalib_etl_yolink_config::{YolinkDevice, YolinkSync};

const DAY: i64 = 86_400_000;
const SIX_HOURS: i64 = 6 * 3_600_000;
const FIVE_MIN: i64 = 300_000;
const STRIDE_DAYS: i64 = 7;

fn day(n: i64) -> i64 {
    Utc.with_ymd_and_hms(2369, 4, 1, 0, 0, 0)
        .unwrap()
        .timestamp_millis()
        + n * DAY
}

/// The tables the download fills. The `_bookkeeping` sidecars and
/// `problems` are left out: a run that was cut off has more attempts
/// than one that was not, and a problem's first-seen stamp is the
/// clock's.
const TABLES: &[&str] = &["yolink_devices", "yolink_readings", "coverage"];

fn devices() -> Vec<YolinkDevice> {
    [
        ("warp-core-coolant", "0123456789abcdef0123456789abcdef"),
        ("cargo-bay-2", "fedcba9876543210fedcba9876543210"),
    ]
    .into_iter()
    .map(|(name, family)| YolinkDevice {
        name: name.into(),
        kind: "watermeter".into(),
        start: "2369-04-01".into(),
        family_device_id: family.into(),
        device_udid: "00112233445566778899aabbccddeeff".into(),
    })
    .collect()
}

fn config() -> YolinkSync {
    YolinkSync {
        overlap_minutes: None,
        window_days: Some(STRIDE_DAYS),
        devices: devices(),
    }
}

/// A meter's readings: every six hours from three hours past the start
/// until `until`, plus any `late` ones.
fn readings(until: i64, late: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = (0..)
        .map(|i| day(0) + 3 * 3_600_000 + i * SIX_HOURS)
        .take_while(|ts| *ts < until)
        .collect();
    out.extend_from_slice(late);
    out.sort_unstable();
    out
}

fn csv(readings: &[i64], from: i64, to: i64) -> String {
    let mut body = String::from("Device Id,Time,Water Meter(GAL),Water Consumption(GAL)\n");
    for ts in readings.iter().filter(|ts| (from..to).contains(ts)) {
        let t = Utc.timestamp_millis_opt(*ts).unwrap();
        body.push_str(&format!(
            "d88b,{},{}.0,1.0\n",
            t.format("%Y/%m/%d %H:%M:%S+0000"),
            (ts - day(0)) / SIX_HOURS
        ));
    }
    body
}

/// Write the answer to every window a walk of `[lo, hi]` asks for.
fn tape(root: &Path, readings: &[i64], lo: i64, hi: i64) {
    for dev in devices() {
        for window in windows_of(lo, hi, STRIDE_DAYS * DAY) {
            let (from, to) = requested(window, FIVE_MIN, day(0));
            let req = window_request(&dev, from, to).unwrap();
            let resp = HttpResponse {
                status: 200,
                headers: Default::default(),
                body: csv(readings, from, to).into_bytes(),
                duration_ms: 0,
            };
            write_fixture(root, &req, &resp).unwrap();
        }
    }
}

struct Yolink {
    playback: PathBuf,
    now_ms: i64,
    /// A store an earlier run left, which every run of this rig starts
    /// from in place of an empty one.
    earlier: Option<PathBuf>,
}

#[async_trait]
impl Rig for Yolink {
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
                sync: config(),
                now_ms: self.now_ms,
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
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test").await?;
        db.close().await;
        Ok(())
    }

    async fn contents(&self, dir: &Path) -> Result<String> {
        let reader = RawDb::open_reader(&db_path_for(dir), None)
            .await?
            .expect("a sealed store has a commit to read");
        let out = dump_tables(reader.pool(), TABLES).await;
        reader.close().await;
        out
    }
}

fn every(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let playback = d.path().join("playback");
    tape(
        &playback,
        &readings(day(20) - 10 * 60_000, &[]),
        day(0),
        day(20),
    );
    let rig = Yolink {
        playback,
        now_ms: day(20),
        earlier: None,
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
}

/// A store an earlier run filled, and ten more days upstream since,
/// with one reading that arrived after that run inside the overlap
/// the next run re-asks. From an empty store nothing is ever already
/// held, so a download that reads its newest row as "fetched up to
/// here" passes the test above and fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_download_cut_off_at_any_request_resumes_to_the_same_store() {
    let d = tempfile::tempdir().unwrap();
    let before = d.path().join("playback-before");
    tape(
        &before,
        &readings(day(20) - 10 * 60_000, &[]),
        day(0),
        day(20),
    );
    let first = Yolink {
        playback: before,
        now_ms: day(20),
        earlier: None,
    };
    let earlier = d.path().join("earlier");
    std::fs::create_dir_all(&earlier).unwrap();
    let db = first.open(&earlier).await.unwrap();
    first.download(&db, StopFlag::new()).await.unwrap();
    first.seal(db).await.unwrap();

    let after = d.path().join("playback-after");
    let late = day(20) - 2 * 60_000;
    tape(
        &after,
        &readings(day(30) - 10 * 60_000, &[late]),
        day(20),
        day(30),
    );
    let rig = Yolink {
        playback: after,
        now_ms: day(30),
        earlier: Some(earlier),
    };
    for how in [How::Kill, How::Stop] {
        let scratch = d.path().join(format!("cuts-{how:?}"));
        every_cut_resumes(&rig, how, &scratch, every)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e:#}"));
    }
    let whole = d.path().join("cuts-Kill").join("whole");
    let got = rig.contents(&whole).await.unwrap();
    assert!(
        got.contains(&format!("\"ts_ms\":{late}")),
        "the late reading, inside the overlap, landed:\n{got}"
    );
}
