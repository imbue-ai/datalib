//! Where the per-day walk picks up. A day that failed is asked for
//! again on the next run even once the walk has resumed past it, and
//! the days between the failed ones are not; a stop is not a failure
//! of the day it interrupted and leaves the cursor before it.

use std::sync::Arc;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_garmin::ingest::daily_path;

use crate::prune_gate::{status, Account, PLAYBACK};

const DISPLAY_NAME: &str = "jean-luc.picard";

/// A day that failed lies behind where the next run resumes (the
/// cursor less the refresh week). Before the retry list it was never
/// asked for again, and its `problems` row stayed for good.
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

/// Asks the step to stop the moment the walk announces one day.
struct StopAt {
    message: &'static str,
    stop: StopFlag,
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
/// remaining metric, and each metric's cursor moved past days it never
/// fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_is_no_problem_and_leaves_the_cursor_before_the_stopped_day() {
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
            "SELECT COUNT(*) FROM sync_scope_state \
             WHERE scope = 'garmin:daily:sleep' AND last_seen_at_utc = '2369-04-04'"
        )
        .await,
        1,
        "the cursor stops at the last day fetched"
    );
    assert_eq!(
        a.count("SELECT COUNT(*) FROM sync_scope_state WHERE scope = 'garmin:daily:stress'")
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
