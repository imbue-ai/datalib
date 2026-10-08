//! The FIT files and wellness bundles: each edge points at its own
//! record's bytes, a file that failed is fetched again however old its
//! record, an answer that held no file is not asked for twice, and an
//! edge an earlier build pointed at another record's file is fetched
//! again too.

use std::sync::Arc;

use datalib_etl::blob_cas::blake3_hex;
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;

use crate::prune_gate::{bytes, status, Account, PLAYBACK, TODAY};
use crate::retry_and_stop::StopAt;

const FIT_13: &str = "/download-service/files/activity/17010413001";

fn hash(b: &str) -> Option<String> {
    Some(blake3_hex(b.as_bytes()))
}

fn fit_edges() -> Vec<(String, Option<String>)> {
    vec![
        ("17010413001".into(), hash(".FIT synthetic 17010413001")),
        ("17010414002".into(), hash(".FIT ride of 2369-04-14")),
    ]
}

const FIT_EDGES_SQL: &str =
    "SELECT activity_id, blake3 FROM garmin_activity_files ORDER BY activity_id";

const SHARE_THE_RIDES_FIT: &str = "UPDATE garmin_activity_files SET blake3 = \
     (SELECT blake3 FROM garmin_activity_files WHERE activity_id = '17010414002')";

/// What a store written before the repair looks like: no record of it.
const NOT_YET_REPAIRED: &str =
    "DELETE FROM sync_scope_state WHERE scope LIKE 'garmin:shared_hash_repair:%'";

/// The CAS bundle is keyed by ref, and every FIT went in under the one
/// ref `fit`: each activity's edge got the hash of the last file of
/// the batch, and the earlier files never reached the CAS. A store
/// written that way is mended by fetching the shared ones again — once:
/// a hash two activities share after that is theirs, and fetching it
/// every run would only get the same bytes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_activitys_fit_edge_points_at_its_own_file() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    let s = a.run().await;
    assert_eq!(s.activity_files, 2, "{}", s.line());
    assert_eq!(a.pairs(FIT_EDGES_SQL).await, fit_edges());

    a.exec(SHARE_THE_RIDES_FIT).await;
    a.exec(NOT_YET_REPAIRED).await;
    let s = a.run().await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert_eq!(
        s.activity_files,
        2,
        "both edges of the shared hash are fetched again: {}",
        s.line()
    );
    assert_eq!(a.pairs(FIT_EDGES_SQL).await, fit_edges());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);

    a.exec(SHARE_THE_RIDES_FIT).await;
    let s = a.run().await;
    assert_eq!(
        s.activity_files,
        0,
        "a store already repaired is not repaired again: {}",
        s.line()
    );
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
}

/// The repair stamps edges failed so a walk fetches them again; with that
/// walk off, nothing would, and the rows would stand for good. So it
/// waits until the walk is on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repair_waits_for_the_walk_that_would_refetch() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.run().await;
    a.exec(SHARE_THE_RIDES_FIT).await;
    a.exec(NOT_YET_REPAIRED).await;

    a.api.activity_files = Some(false);
    let s = a.run().await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);

    a.api.activity_files = None;
    let s = a.run().await;
    assert_eq!(s.activity_files, 2, "{}", s.line());
    assert_eq!(a.pairs(FIT_EDGES_SQL).await, fit_edges());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
}

/// Answer the activity listing asked from `day` with the spec's
/// activities at `which`: a start the synthesizer wrote no fixture for.
fn answer_listing_from(a: &Account, day: &str, which: &[usize]) {
    let listed: Vec<serde_json::Value> = which
        .iter()
        .map(|i| a.spec["activities"][*i]["listing"].clone())
        .collect();
    a.answer(
        &format!(
            "/activitylist-service/activities/search/activities?start=0&limit=100&startDate={day}"
        ),
        status(200, &serde_json::Value::Array(listed).to_string()),
    );
}

/// With a refresh window of one day the listing starts on the 14th, the
/// day after the first activity.
fn list_only_the_14th(a: &mut Account) {
    a.api.refresh_days = Some(1);
    answer_listing_from(a, "2369-04-14", &[1]);
}

/// A FIT was sought only for an activity the run's listing named, so
/// one that failed and then fell behind the listing window was never
/// asked for again, and its `problems` row stood for good.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_fit_behind_the_listing_window_is_fetched_again() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.answer_bytes(FIT_13, status(500, "upstream fell over"));
    let s1 = a.run().await;
    assert_eq!(s1.errors, 1, "{}", s1.line());
    assert_eq!(
        a.problems().await.keys().collect::<Vec<_>>(),
        ["garmin_activity_files:17010413001#fit"]
    );

    a.resynthesize();
    list_only_the_14th(&mut a);
    let s2 = a.run().await;
    assert_eq!(s2.errors, 0, "{}", s2.line());
    assert_eq!(s2.activities_listed, 1, "{}", s2.line());
    assert_eq!(s2.activity_files, 1, "{}", s2.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
    assert_eq!(a.pairs(FIT_EDGES_SQL).await, fit_edges());
}

fn wellness(day: &str) -> String {
    format!("/download-service/files/wellness/{day}")
}

const WELLNESS_EDGES_SQL: &str =
    "SELECT calendar_date, blake3 FROM garmin_wellness_files ORDER BY calendar_date";

const WELLNESS_DAYS_HELD_SQL: &str = "SELECT COUNT(*) FROM garmin_wellness_files f \
     JOIN garmin_wellness_files_bookkeeping b ON b.id = f.id \
     WHERE f.blake3 IS NULL AND b.held_version IS NOT NULL";

/// A day whose bundle failed before the refresh window is fetched
/// again, and a day Garmin had no bundle for is a row saying so, which
/// once settled is not asked for again. And, as with the FIT files,
/// every bundle of a batch once shared one ref.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_wellness_day_is_fetched_again_and_a_day_with_no_bundle_is_held() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.api.wellness_files = Some(true);
    for day in 1..=15 {
        a.answer_bytes(&wellness(&format!("2369-04-{day:02}")), status(404, ""));
    }
    a.answer_bytes(&wellness("2369-04-02"), bytes(b"bundle of 2369-04-02"));
    a.answer_bytes(&wellness("2369-04-03"), status(500, "upstream fell over"));
    a.answer_bytes(&wellness("2369-04-04"), bytes(b"bundle of 2369-04-04"));
    let s1 = a.run().await;
    assert_eq!(s1.errors, 1, "{}", s1.line());
    assert_eq!(s1.wellness_files, 2, "{}", s1.line());
    assert_eq!(
        a.problems().await.keys().collect::<Vec<_>>(),
        ["garmin_wellness_files:2369-04-03#wellness_zip"]
    );
    assert_eq!(
        a.pairs(WELLNESS_EDGES_SQL).await[1..3],
        [
            ("2369-04-02".to_string(), hash("bundle of 2369-04-02")),
            ("2369-04-04".to_string(), hash("bundle of 2369-04-04")),
        ],
        "a day whose request failed has no row: nothing was answered for it"
    );
    assert_eq!(
        a.count(WELLNESS_DAYS_HELD_SQL).await,
        12,
        "each day with no bundle is a row, held; the day that failed is not"
    );

    a.answer_bytes(&wellness("2369-04-03"), bytes(b"bundle of 2369-04-03"));
    let s2 = a.run().await;
    assert_eq!(s2.errors, 0, "{}", s2.line());
    assert_eq!(s2.wellness_files, 1, "{}", s2.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
    assert_eq!(
        a.pairs(WELLNESS_EDGES_SQL).await[2],
        ("2369-04-03".to_string(), hash("bundle of 2369-04-03"))
    );

    let s3 = a.run().await;
    assert_eq!(
        s2.requests,
        s3.requests + 1,
        "the retry cost the failed day and no other: a settled day with no bundle is not \
         asked for again: {} vs {}",
        s2.line(),
        s3.line()
    );
}

/// G6. A bundle fetched on its own day is that day so far. It was
/// stored and, being stored, never asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wellness_bundle_fetched_before_its_day_was_over_is_fetched_again() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.api.wellness_files = Some(true);
    a.api.refresh_days = Some(0);
    a.spec["today"] = "2369-04-14".into();
    a.spec["wellness"] = serde_json::json!({"2369-04-14": "the 14th, by noon"});
    a.resynthesize();
    let s1 = a.run_on(TODAY.pred_opt().unwrap()).await;
    assert_eq!((s1.errors, s1.wellness_files), (0, 1), "{}", s1.line());

    a.spec["today"] = "2369-04-15".into();
    a.spec["wellness"] = serde_json::json!({"2369-04-14": "the 14th, whole"});
    a.resynthesize();
    answer_listing_from(&a, "2369-04-14", &[1]);
    answer_listing_from(&a, "2369-04-15", &[]);
    let s2 = a.run().await;
    assert_eq!((s2.errors, s2.wellness_files), (0, 1), "{}", s2.line());
    assert_eq!(
        a.pairs(WELLNESS_EDGES_SQL).await[13],
        ("2369-04-14".to_string(), hash("the 14th, whole"))
    );

    let s3 = a.run().await;
    assert_eq!(
        s3.wellness_files,
        0,
        "a bundle fetched after its day ended is final: {}",
        s3.line()
    );
}

/// G3. Files turned on after the activities were mirrored: a file was
/// sought only for an activity the run's listing named, so those older
/// than the refresh window never got one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_turned_on_later_are_fetched_for_activities_behind_the_listing_window() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.api.activity_files = Some(false);
    let s1 = a.run().await;
    assert_eq!((s1.errors, s1.activity_files), (0, 0), "{}", s1.line());

    a.api.activity_files = None;
    list_only_the_14th(&mut a);
    let s2 = a.run().await;
    assert_eq!(s2.activities_listed, 1, "{}", s2.line());
    assert_eq!((s2.errors, s2.activity_files), (0, 2), "{}", s2.line());
    assert_eq!(a.pairs(FIT_EDGES_SQL).await, fit_edges());
}

/// An activity entered by hand has no file. That answer is held for the
/// listing as it reads, so the file is not asked for run after run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_activity_with_no_file_is_asked_once() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    a.answer_bytes(FIT_13, status(404, ""));
    let s1 = a.run().await;
    assert_eq!((s1.errors, s1.activity_files), (0, 1), "{}", s1.line());
    assert_eq!(
        a.pairs(FIT_EDGES_SQL).await[0],
        ("17010413001".to_string(), None)
    );
    let s2 = a.run().await;
    let s3 = a.run().await;
    assert_eq!(s2.requests, s3.requests, "{} vs {}", s2.line(), s3.line());
    assert_eq!(
        s1.requests - s2.requests,
        20 * 8 + 2 + 2,
        "the first run's settled days, two details and two file requests; none after it: {} vs {}",
        s1.line(),
        s2.line()
    );
}

/// A download that is not a zip comes back the same every time, so it is
/// not asked for again run after run; an edit to the activity is the
/// one thing that could change it. The fetch landed, so the loss is a
/// warning on the record, not a failure of the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_fit_is_fetched_again_only_when_its_activity_changes() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.answer_bytes(FIT_13, bytes(b"not a zip, captain"));
    let s1 = a.run().await;
    assert_eq!((s1.errors, s1.activity_files), (0, 1), "{}", s1.line());
    let key = "garmin_activity_files:17010413001#fit";
    assert_eq!(a.problems().await.keys().collect::<Vec<_>>(), [key]);
    assert_eq!(
        a.count(
            "SELECT COUNT(*) FROM problems \
             WHERE scope_key = 'garmin_activity_files:17010413001#fit' AND severity = 'warning'"
        )
        .await,
        1,
        "held with something lost is a warning"
    );

    let s2 = a.run().await;
    assert_eq!(
        (s2.errors, s2.activity_files),
        (0, 0),
        "not asked for again: {}",
        s2.line()
    );
    assert_eq!(a.problems().await.keys().collect::<Vec<_>>(), [key]);

    a.spec["activities"][0]["listing"]["activityName"] = "Holodeck run: Dixon Hill, again".into();
    a.resynthesize();
    let s3 = a.run().await;
    assert_eq!((s3.errors, s3.activity_files), (0, 1), "{}", s3.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
}

/// G2. The activity of an unreadable FIT changes, and the run that lists
/// the change is stopped before it reaches the file. That the activity
/// had changed was known only to that run, and the listing was already
/// stored as it now reads, so the file was never asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_fit_whose_activity_changed_in_a_stopped_run_is_fetched_by_the_next() {
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    a.answer_bytes(FIT_13, bytes(b"not a zip, captain"));
    let s1 = a.run().await;
    assert_eq!((s1.errors, s1.activity_files), (0, 1), "{}", s1.line());
    assert_eq!(
        a.problems().await.keys().collect::<Vec<_>>(),
        ["garmin_activity_files:17010413001#fit"]
    );

    a.spec["activities"][0]["listing"]["activityName"] = "Holodeck run: Dixon Hill, again".into();
    a.resynthesize();
    let stop = StopFlag::new();
    let control = DownloadControl {
        stop: stop.clone(),
        ..Default::default()
    };
    let progress = Progress::new(Arc::new(StopAt {
        message: "garmin: activity 17010413001",
        stop,
    }));
    let s2 = a.run_with(control, progress).await;
    assert_eq!(
        s2.activity_files,
        0,
        "stopped before the file: {}",
        s2.line()
    );

    let s3 = a.run().await;
    assert_eq!((s3.errors, s3.activity_files), (0, 1), "{}", s3.line());
    assert!(a.problems().await.is_empty(), "{:?}", a.problems().await);
    assert_eq!(a.pairs(FIT_EDGES_SQL).await, fit_edges());
}
