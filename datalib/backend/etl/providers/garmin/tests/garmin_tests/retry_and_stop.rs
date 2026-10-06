//! What the next run asks for again. A day that failed is fetched again
//! however old, and the days around it that fetched are not; an
//! activity detail that failed is fetched again, whether its listing
//! changed or not; a stop is not a failure of the day it interrupted;
//! a `since` moved earlier is walked once.

use std::sync::Arc;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_garmin::ingest::daily_path;

use crate::prune_gate::{status, Account, PLAYBACK, TODAY};

const DISPLAY_NAME: &str = "jean-luc.picard";

/// A day that failed lies before the refresh week. It has no row, so
/// it is owed, and the settled days around it are not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_day_behind_the_resume_point_is_fetched_again_and_only_it() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    a.answer(
        &daily_path("sleep", DISPLAY_NAME, "2369-04-03"),
        status(500, "upstream fell over"),
    );
    let s1 = a.run().await;
    assert_eq!(s1.errors, 1, "{}", s1.line());
    assert_eq!(
        a.problems().await.keys().collect::<Vec<_>>(),
        ["garmin_daily:sleep#2369-04-03"]
    );

    a.resynthesize();
    let s2 = a.run().await;
    assert_eq!(s2.errors, 0, "{}", s2.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_daily WHERE id = 'sleep#2369-04-03'")
            .await,
        1
    );

    let s3 = a.run().await;
    assert_eq!(
        s2.requests,
        s3.requests + 1,
        "the retry costs the failed day and no other: {} vs {}",
        s2.line(),
        s3.line()
    );
}

/// With no `since` in the config the window starts a year before the
/// first run and stays there. Taken afresh each run it moved a day a
/// day, and a day that failed near its start fell out behind it: never
/// asked for again, its `problems` row there for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_window_stays_where_the_first_run_put_it() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    // The clock reads a year after the fixture's first day, so the
    // default window opens on it; `until` keeps the walk inside the
    // fixture.
    let first_day = "2369-04-03";
    let first_run = chrono::NaiveDate::parse_from_str(first_day, "%Y-%m-%d").unwrap()
        + chrono::Duration::days(365);
    a.spec["since"] = first_day.into();
    a.resynthesize();
    a.api.since = None;
    a.api.until = Some(TODAY.format("%Y-%m-%d").to_string());
    a.api.metrics = Some(vec!["sleep".into()]);
    a.answer(
        &daily_path("sleep", DISPLAY_NAME, first_day),
        status(500, "upstream fell over"),
    );
    let s1 = a.run_on(first_run).await;
    assert_eq!(s1.errors, 1, "{}", s1.line());
    let key = format!("garmin_daily:sleep#{first_day}");
    assert_eq!(a.problems().await.keys().collect::<Vec<_>>(), [&key]);

    a.resynthesize();
    let s2 = a.run_on(first_run + chrono::Duration::days(3)).await;
    assert_eq!(s2.errors, 0, "{}", s2.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
    assert_eq!(
        a.count(
            "SELECT COUNT(*) FROM garmin_daily_bookkeeping \
             WHERE id = 'sleep#2369-04-03' AND last_error IS NULL AND fetched_at_utc IS NOT NULL"
        )
        .await,
        1,
        "the failed day was fetched again, not just forgotten"
    );
}

/// A `since` moved past a failed day takes that day out of the window:
/// no run will fetch it, so its `problems` row goes rather than
/// standing for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_day_the_window_moved_past_is_no_longer_a_problem() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.api.metrics = Some(vec!["sleep".into()]);
    a.answer(
        &daily_path("sleep", DISPLAY_NAME, "2369-04-03"),
        status(500, "upstream fell over"),
    );
    let s1 = a.run().await;
    assert_eq!(s1.errors, 1, "{}", s1.line());

    a.spec["since"] = "2369-04-05".into();
    a.resynthesize();
    a.answer(
        &daily_path("sleep", DISPLAY_NAME, "2369-04-03"),
        status(500, "upstream fell over"),
    );
    a.api.since = Some("2369-04-05".into());
    let s2 = a.run().await;
    assert_eq!(
        s2.errors,
        0,
        "the day is not asked for again: {}",
        s2.line()
    );
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
}

/// An activity's detail was fetched only when its listing entry
/// changed, and a failed fetch leaves the listing stored as it came, so
/// the detail was never asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_activity_detail_is_fetched_again_though_the_listing_did_not_change() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    let detail = "/activity-service/activity/17010413001";
    a.answer(detail, status(500, "upstream fell over"));
    let s1 = a.run().await;
    assert_eq!(s1.errors, 1, "{}", s1.line());
    assert_eq!(
        a.problems().await.keys().collect::<Vec<_>>(),
        ["garmin_activity_details:17010413001"]
    );

    a.resynthesize();
    let s2 = a.run().await;
    assert_eq!(s2.errors, 0, "{}", s2.line());
    assert_eq!(s2.activities_fetched, 1, "{}", s2.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);

    let s3 = a.run().await;
    assert_eq!(
        s3.activities_fetched,
        0,
        "a stored detail is left alone: {}",
        s3.line()
    );
}

/// Asks the step to stop the moment the walk announces one unit of work.
pub(crate) struct StopAt {
    pub(crate) message: &'static str,
    pub(crate) stop: StopFlag,
}

impl ProgressSink for StopAt {
    fn set_message(&self, msg: &str) {
        if msg == self.message {
            self.stop.request();
        }
    }
}

/// A stop mid-walk makes every request after it fail at once. Those
/// failures used to be recorded as fetch failures of ten days of every
/// remaining metric.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_is_no_problem_and_the_days_after_it_are_fetched_next_run() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    let stop = StopFlag::new();
    let control = DownloadControl {
        stop: stop.clone(),
        ..Default::default()
    };
    let progress = Progress::new(Arc::new(StopAt {
        message: "garmin: sleep 2369-04-05",
        stop,
    }));
    let s = a.run_with(control, progress).await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
    assert_eq!(
        a.count(
            "SELECT COUNT(*) FROM garmin_daily \
             WHERE metric = 'sleep' AND calendar_date <= '2369-04-04'"
        )
        .await,
        4,
        "the days fetched before the stop are stored"
    );
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_daily WHERE metric = 'sleep'")
            .await,
        4,
        "and no day after it"
    );
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_daily WHERE metric = 'stress'")
            .await,
        0,
        "a metric after the stop is not walked"
    );
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_weigh_ins").await,
        0,
        "a phase after the stop is not started"
    );

    let s = a.run().await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert!(a.problems().await.is_empty());
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_daily WHERE metric = 'sleep'")
            .await,
        15
    );
}

/// G1. An activity is renamed upstream and the refetch of its detail
/// fails. The listing row was already stored as it now reads, and the
/// only record that it had changed was a set in memory, so no later run
/// asked for the detail again: it stayed as it was before the rename.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_activity_whose_detail_refetch_fails_is_fetched_again() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.run().await;

    a.spec["activities"][0]["listing"]["activityName"] = "Holodeck run: Dixon Hill, again".into();
    a.spec["activities"][0]["detail"]["activityName"] = "Holodeck run: Dixon Hill, again".into();
    a.resynthesize();
    a.answer(
        "/activity-service/activity/17010413001",
        status(500, "upstream fell over"),
    );
    let s2 = a.run().await;
    assert_eq!((s2.errors, s2.activities_fetched), (1, 0), "{}", s2.line());

    a.resynthesize();
    let s3 = a.run().await;
    assert_eq!((s3.errors, s3.activities_fetched), (0, 1), "{}", s3.line());
    assert_eq!(
        a.count(
            "SELECT COUNT(*) FROM garmin_activity_details d \
             JOIN garmin_activities a ON a.id = d.id AND a.listing_hash = d.listing_hash \
             WHERE d.id = '17010413001' \
               AND json_extract(json(d.payload), '$.activityName') = 'Holodeck run: Dixon Hill, again'"
        )
        .await,
        1,
        "the detail is the renamed activity's, held for the listing as it now reads"
    );
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
}

/// G4. `since` is moved earlier and one of the newly wanted days fails
/// every time. The record of the config a walk ran under was written
/// only by a run with no errors, so every later run saw `since` as just
/// widened and walked all of history again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widened_since_is_walked_once_though_one_day_keeps_failing() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.api.metrics = Some(vec!["sleep".into()]);
    a.spec["activities"][0]["listing"]["startTimeGMT"] = "2369-04-03 06:30:00".into();
    a.spec["since"] = "2369-04-10".into();
    a.resynthesize();
    a.api.since = Some("2369-04-10".into());
    let s1 = a.run().await;
    assert_eq!((s1.errors, s1.days), (0, 6), "{}", s1.line());
    assert_eq!(s1.activities_listed, 1, "{}", s1.line());

    a.spec["since"] = "2369-04-01".into();
    a.resynthesize();
    a.api.since = Some("2369-04-01".into());
    let failing = daily_path("sleep", DISPLAY_NAME, "2369-04-03");
    a.answer(&failing, status(500, "upstream fell over"));
    let s2 = a.run().await;
    assert_eq!(s2.errors, 1, "{}", s2.line());
    assert_eq!(
        s2.days,
        8 + 6,
        "the nine new days less the one that failed, and the refresh window: {}",
        s2.line()
    );
    assert_eq!(
        s2.activities_listed,
        2,
        "the listing reaches back to the new start: {}",
        s2.line()
    );
    assert_eq!(a.count("SELECT COUNT(*) FROM garmin_weigh_ins").await, 4);

    let s3 = a.run().await;
    assert_eq!(s3.errors, 1, "{}", s3.line());
    assert_eq!(
        s3.days,
        7,
        "only the days not yet settled; the failing day is one more request: {}",
        s3.line()
    );
    assert_eq!(
        s3.activities_listed,
        1,
        "the listing is back to the refresh window: {}",
        s3.line()
    );
}
