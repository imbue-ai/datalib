//! The prune gate. A listing that did not come back as an enumeration —
//! a wrapped object with no array inside, a 204/404/empty body, a page
//! walk that hit an error — leaves the stored rows alone and leaves a
//! `problems` row saying so; a phase that fails wholesale leaves its
//! row and the run stays green. The next run that lists clears the row
//! and prunes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datalib_etl::control::DownloadControl;
use datalib_etl::http::{HttpResponse, PLAYBACK_ENV};
use datalib_etl::progress::Progress;
use datalib_etl::retry::{self, RetryGuard};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{write_fixture, Synthesizer};
use datalib_etl_garmin::auth::Credentials;
use datalib_etl_garmin::ingest::api::{base_url, req_get};
use datalib_etl_garmin::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_garmin::synthesize::GarminSynth;
use datalib_etl_garmin_config::GarminApi;
use serde_json::{json, Value};

/// `PLAYBACK_ENV` is process-global; the tests in this binary take turns.
pub(crate) static PLAYBACK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn spec_path() -> PathBuf {
    let rel = "datalib/backend/etl/providers/garmin/tests/fixtures/garmin_tng/tng.json";
    if Path::new(rel).exists() {
        return PathBuf::from(rel);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/garmin_tng/tng.json")
}

pub(crate) struct Account {
    _dir: tempfile::TempDir,
    playback: PathBuf,
    raw: PathBuf,
    spec_file: PathBuf,
    pub(crate) spec: Value,
    pub(crate) api: GarminApi,
}

impl Account {
    pub(crate) fn tng() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let playback = dir.path().join("playback");
        let raw = dir.path().join("garmin").join("ingest");
        std::fs::create_dir_all(&raw).unwrap();
        let spec: Value = serde_json::from_slice(&std::fs::read(spec_path()).unwrap()).unwrap();
        let spec_file = dir.path().join("spec.json");
        let account = Self {
            _dir: dir,
            playback,
            raw,
            spec_file,
            spec,
            api: GarminApi {
                since: Some("2369-04-01".into()),
                ..Default::default()
            },
        };
        account.resynthesize();
        account
    }

    /// Rewrite every fixture from the (possibly edited) spec.
    pub(crate) fn resynthesize(&self) {
        std::fs::write(&self.spec_file, serde_json::to_vec(&self.spec).unwrap()).unwrap();
        GarminSynth::new(&self.spec_file)
            .synthesize(&self.playback)
            .unwrap();
    }

    /// Make one request answer differently from what the spec says.
    pub(crate) fn answer(&self, path: &str, resp: HttpResponse) {
        let req = req_get(&format!("{}{path}", base_url("garmin.com")));
        write_fixture(&self.playback, &req, &resp).unwrap();
    }

    pub(crate) async fn run(&self) -> FetchSummary {
        self.run_with(DownloadControl::default(), Progress::noop())
            .await
    }

    /// A run whose transport honours `control.stop`, as the step
    /// driver's retry scope makes it in production.
    pub(crate) async fn run_with(
        &self,
        control: DownloadControl,
        progress: Progress,
    ) -> FetchSummary {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        let db = RawDb::open(&db_path_for(&self.raw)).await.unwrap();
        let fast = std::time::Duration::from_millis(1);
        let guard = RetryGuard::new(
            std::time::Duration::from_secs(3600),
            100,
            fast,
            fast,
            control.stop.clone(),
        );
        let summary = retry::scope(
            guard,
            fetch(FetchOptions {
                db: db.clone(),
                creds: Credentials::fixed("playback"),
                api: self.api.clone(),
                today: chrono::NaiveDate::from_ymd_opt(2369, 4, 15).unwrap(),
                progress,
                control,
                sealer: None,
            }),
        )
        .await;
        db.commit_all("test").await.unwrap();
        db.close().await;
        summary.unwrap()
    }

    pub(crate) async fn count(&self, sql: &'static str) -> i64 {
        let pool = datalib_pin::open_reader(&db_path_for(&self.raw))
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
        pool.close().await;
        n
    }

    /// Every fetch-stage `problems` row, `scope_key → sample`.
    pub(crate) async fn problems(&self) -> BTreeMap<String, String> {
        let pool = datalib_pin::open_reader(&db_path_for(&self.raw))
            .await
            .unwrap();
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT scope_key, sample FROM problems WHERE stage = 'fetch' ORDER BY scope_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        pool.close().await;
        rows.into_iter().collect()
    }

    pub(crate) async fn set_cursor(&self, scope: &str, value: &str) {
        let db = RawDb::open(&db_path_for(&self.raw)).await.unwrap();
        db.set_cursor(scope, value).await.unwrap();
        db.commit_all("test").await.unwrap();
        db.close().await;
    }
}

pub(crate) fn status(code: u16, body: &str) -> HttpResponse {
    let mut headers = BTreeMap::new();
    headers.insert("content-type".into(), "application/json".into());
    HttpResponse {
        status: code,
        headers,
        body: body.as_bytes().to_vec(),
        duration_ms: 0,
    }
}

const BADGES: &str = "/badge-service/badge/earned";
const PERSONAL_RECORDS: &str = "/personalrecord-service/personalrecord/prs/jean-luc.picard";
const DEVICES: &str = "/device-service/deviceregistration/devices";

/// A 200 whose object wraps no array is not an enumeration: the badge
/// stays, and the run says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrapped_listing_with_no_array_does_not_prune() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    let s1 = a.run().await;
    assert_eq!(s1.errors, 0, "{}", s1.line());
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'badges'")
            .await,
        1
    );

    a.answer(
        BADGES,
        status(200, &json!({"status": "ok", "count": 0}).to_string()),
    );
    let s2 = a.run().await;
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'badges'")
            .await,
        1,
        "a listing with no array inside must not prune: {}",
        s2.line()
    );
    assert_eq!(s2.errors, 1, "{}", s2.line());
    let problems = a.problems().await;
    assert_eq!(
        problems.keys().collect::<Vec<_>>(),
        ["listing:badges"],
        "{problems:?}"
    );

    // The listing answers again: the row goes, and nothing is pruned
    // because nothing left.
    a.resynthesize();
    let s3 = a.run().await;
    assert_eq!(s3.errors, 0, "{}", s3.line());
    assert!(a.problems().await.is_empty());
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'badges'")
            .await,
        1
    );
}

/// 204, 404 and an empty 200 body all reach the walk as "nothing", and
/// nothing is not an enumeration — for the account listings and for
/// the device registration alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_answering_nothing_does_not_prune() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    a.run().await;
    assert_eq!(a.count("SELECT COUNT(*) FROM garmin_devices").await, 1);

    for (label, nothing) in [
        ("204", status(204, "")),
        ("404", status(404, r#"{"message": "not found"}"#)),
        ("empty 200", status(200, "")),
    ] {
        a.answer(PERSONAL_RECORDS, nothing.clone());
        a.answer(DEVICES, nothing);
        let s = a.run().await;
        assert_eq!(
            a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'personal_records'")
                .await,
            1,
            "{label}: a listing answering nothing must not prune: {}",
            s.line()
        );
        assert_eq!(
            a.count("SELECT COUNT(*) FROM garmin_devices").await,
            1,
            "{label}: a device listing answering nothing must not prune: {}",
            s.line()
        );
        assert_eq!(s.errors, 2, "{label}: {}", s.line());
        let problems = a.problems().await;
        assert_eq!(
            problems.keys().collect::<Vec<_>>(),
            ["listing:devices", "listing:personal_records"],
            "{label}: {problems:?}"
        );
        assert!(
            problems["listing:personal_records"].contains("nothing"),
            "{label}: the sample says what came back: {problems:?}"
        );
    }

    a.resynthesize();
    let s = a.run().await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert!(a.problems().await.is_empty());
}

/// A phase that fails wholesale — here the weight walk, handed a cursor
/// it cannot parse — is a `problems` row, not only a log line, and the
/// run still returns `Ok` so the other phases' work is committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phase_that_fails_wholesale_is_a_problems_row_and_the_run_stays_green() {
    let _serial = PLAYBACK.lock().await;
    let a = Account::tng();
    a.run().await;
    assert_eq!(a.count("SELECT COUNT(*) FROM garmin_weigh_ins").await, 4);

    a.set_cursor("garmin:weight", "stardate 46944.2").await;
    let s = a.run().await;
    assert_eq!(s.errors, 1, "{}", s.line());
    assert_eq!(s.weigh_ins, 0, "the phase never listed: {}", s.line());
    assert_eq!(
        s.items,
        4,
        "the phases after the failed one still ran: {}",
        s.line()
    );
    assert_eq!(a.count("SELECT COUNT(*) FROM garmin_weigh_ins").await, 4);
    let problems = a.problems().await;
    assert_eq!(
        problems.keys().collect::<Vec<_>>(),
        ["phase:weight"],
        "{problems:?}"
    );
    assert!(
        problems["phase:weight"].contains("stardate"),
        "the sample carries the error: {problems:?}"
    );

    a.set_cursor("garmin:weight", "2369-04-15").await;
    let s = a.run().await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert!(a.problems().await.is_empty());
}

/// A workout listing that stops mid-walk is not an enumeration: the
/// page that failed may have named the rows the first page did not. A
/// walk that reaches its end is, and prunes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_page_that_fails_does_not_prune_and_a_complete_walk_does() {
    use datalib_etl_garmin::ingest::{item_listing_path, ITEM_PAGE};
    let _serial = PLAYBACK.lock().await;
    let mut a = Account::tng();
    let workouts: Vec<Value> = (0..=ITEM_PAGE)
        .map(|i| json!({"workoutId": 4000 + i, "workoutName": format!("Drill {i}")}))
        .collect();
    a.spec["items"]["workouts"] = Value::Array(workouts);
    a.resynthesize();
    let s1 = a.run().await;
    assert_eq!(s1.errors, 0, "{}", s1.line());
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'workouts'")
            .await,
        (ITEM_PAGE + 1) as i64,
        "the walk reached the second page: {}",
        s1.line()
    );

    let page_two = item_listing_path("workouts", "jean-luc.picard", None, ITEM_PAGE).unwrap();
    a.answer(&page_two, status(500, "upstream fell over"));
    let s2 = a.run().await;
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'workouts'")
            .await,
        (ITEM_PAGE + 1) as i64,
        "a walk whose second page failed must not prune: {}",
        s2.line()
    );
    assert_eq!(s2.items_pruned, 0, "{}", s2.line());
    assert_eq!(s2.errors, 1, "{}", s2.line());
    let problems = a.problems().await;
    assert_eq!(
        problems.keys().collect::<Vec<_>>(),
        ["listing:workouts"],
        "{problems:?}"
    );
    assert!(
        problems["listing:workouts"].contains("offset 100"),
        "the sample names the page that failed: {problems:?}"
    );

    // An endpoint that ignores `start` answers the first page again:
    // the walk stops at the repeat, and prunes nothing.
    let first_page: Vec<Value> =
        a.spec["items"]["workouts"].as_array().unwrap()[..ITEM_PAGE].to_vec();
    a.answer(
        &page_two,
        status(200, &Value::Array(first_page).to_string()),
    );
    let s2b = a.run().await;
    assert_eq!(
        s2b.requests,
        s2.requests,
        "one repeat is enough to stop: {}",
        s2b.line()
    );
    assert_eq!(s2b.items_pruned, 0, "{}", s2b.line());
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'workouts'")
            .await,
        (ITEM_PAGE + 1) as i64,
        "a walk that saw the same page twice must not prune: {}",
        s2b.line()
    );
    assert!(
        a.problems().await["listing:workouts"].contains("repeated"),
        "{:?}",
        a.problems().await
    );

    // Upstream drops the last workout; the walk is one full page and
    // one empty page, reaches its end, and prunes the one that left.
    a.spec["items"]["workouts"].as_array_mut().unwrap().pop();
    a.resynthesize();
    let s3 = a.run().await;
    assert_eq!(s3.errors, 0, "{}", s3.line());
    assert_eq!(s3.items_pruned, 1, "{}", s3.line());
    assert_eq!(
        a.count("SELECT COUNT(*) FROM garmin_items WHERE kind = 'workouts'")
            .await,
        ITEM_PAGE as i64
    );
    assert!(a.problems().await.is_empty());
}
