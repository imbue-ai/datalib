//! Spec → playback fixtures → the real ingest walk → the store: every
//! table lands, a second run changes nothing, and a weigh-in the
//! upstream listing stops naming is pruned.

use std::path::{Path, PathBuf};

use datalib_etl::control::DownloadControl;
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::Progress;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_garmin::auth::Credentials;
use datalib_etl_garmin::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_garmin::synthesize::GarminSynth;
use datalib_etl_garmin_config::{GarminApi, DAILY_METRICS};
use sqlx::Row;

fn spec_path() -> PathBuf {
    let rel = "datalib/backend/etl/providers/garmin/tests/fixtures/garmin_tng/tng.json";
    if Path::new(rel).exists() {
        return PathBuf::from(rel);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/garmin_tng/tng.json")
}

async fn run(raw: &Path, api: &GarminApi) -> FetchSummary {
    let db = RawDb::open(&db_path_for(raw)).await.unwrap();
    let summary = fetch(FetchOptions {
        db: db.clone(),
        creds: Credentials::fixed("playback"),
        api: api.clone(),
        today: chrono::NaiveDate::from_ymd_opt(2369, 4, 15).unwrap(),
        progress: Progress::noop(),
        control: DownloadControl::default(),
        sealer: None,
    })
    .await;
    db.close().await;
    summary.unwrap()
}

/// The test's own store, read back bare after its own run: unpinned is
/// fine here, and what the literal table names in the SQL expect.
async fn count(raw: &Path, sql: &'static str) -> i64 {
    let pool = datalib_pin::open_reader(&db_path_for(raw)).await.unwrap();
    let n: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
    pool.close().await;
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garmin_synth_playback_ingest_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let playback = d.path().join("playback");
    let raw = d.path().join("garmin").join("ingest");
    std::fs::create_dir_all(&raw).unwrap();

    let report = GarminSynth::new(spec_path()).synthesize(&playback).unwrap();
    // 3 account/device + 20 metrics × 15 days + 2 walk windows × (1
    // weight chunk + 1 activity page) + 2 × (detail + zip) + 5 item
    // listings.
    assert_eq!(
        report.fixtures_written,
        3 + DAILY_METRICS.len() * 15 + 4 + 4 + 5
    );
    std::env::set_var(PLAYBACK_ENV, &playback);

    let api = GarminApi {
        since: Some("2369-04-01".into()),
        ..Default::default()
    };
    let s = run(&raw, &api).await;
    assert_eq!(s.errors, 0, "{}", s.line());
    assert_eq!(s.metrics, DAILY_METRICS.len());
    assert_eq!(s.days, DAILY_METRICS.len() * 15);
    assert_eq!(s.weigh_ins, 4);
    assert_eq!(s.activities_listed, 2);
    assert_eq!(s.activities_fetched, 2);
    assert_eq!(s.activity_files, 2);
    assert_eq!(s.devices, 1);
    assert_eq!(s.items, 4);
    assert_eq!(s.wellness_files, 0, "off by default");

    assert_eq!(count(&raw, "SELECT COUNT(*) FROM garmin_account").await, 2);
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM garmin_daily").await,
        (DAILY_METRICS.len() * 15) as i64,
        "every (metric, day) pair is a row, empty days included"
    );
    assert_eq!(
        count(
            &raw,
            "SELECT COUNT(*) FROM garmin_daily WHERE json(payload) <> 'null'"
        )
        .await,
        5,
        "only the spec's days carry a payload"
    );
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM garmin_weigh_ins").await,
        4
    );
    assert_eq!(
        count(
            &raw,
            "SELECT COUNT(*) FROM garmin_weigh_ins WHERE weight_g = 77600.0 AND calendar_date = '2369-04-14'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &raw,
            "SELECT COUNT(*) FROM garmin_activities WHERE activity_type = 'running'"
        )
        .await,
        1
    );
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM garmin_activity_details").await,
        2
    );
    assert_eq!(
        count(
            &raw,
            "SELECT COUNT(*) FROM garmin_activity_files WHERE blake3 IS NOT NULL"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &raw,
            "SELECT COUNT(*) FROM garmin_items WHERE kind = 'gear'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &raw,
            "SELECT COUNT(*) FROM sync_scope_state WHERE scope LIKE 'garmin:%'"
        )
        .await,
        (DAILY_METRICS.len() + 2) as i64,
        "one cursor per metric, plus weight and activities"
    );

    // The FIT bytes are in the CAS, unzipped.
    {
        let cas = datalib_etl::blob_cas::BlobCas::open_reader(
            &datalib_etl::blob_cas::cas_path_for(&db_path_for(&raw)),
        )
        .await
        .unwrap();
        let pool = datalib_pin::open_reader(&db_path_for(&raw)).await.unwrap();
        let row = sqlx::query(
            "SELECT blake3 FROM garmin_activity_files WHERE activity_id = '17010414002'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let hash: String = row.get("blake3");
        pool.close().await;
        let obj = cas.get(&hash).await.unwrap().expect("blob present");
        assert_eq!(obj.bytes, b".FIT ride of 2369-04-14");
        cas.close().await;
    }

    // Second run: everything re-fetched inside the refresh window, and
    // nothing new to fetch beyond it.
    let s2 = run(&raw, &api).await;
    assert_eq!(s2.errors, 0, "{}", s2.line());
    assert_eq!(
        s2.activities_fetched, 0,
        "unchanged listings fetch no detail"
    );
    assert_eq!(s2.activity_files, 0, "a stored FIT file is not re-pulled");
    assert_eq!(
        s2.days,
        DAILY_METRICS.len() * 8,
        "cursor at the 15th, refresh_days=7: the 8th through the 15th again"
    );
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM garmin_weigh_ins").await,
        4
    );

    // A weigh-in the range listing stops naming is pruned.
    let mut spec: serde_json::Value =
        serde_json::from_slice(&std::fs::read(spec_path()).unwrap()).unwrap();
    spec["weigh_ins"].as_array_mut().unwrap().pop();
    let edited = d.path().join("edited.json");
    std::fs::write(&edited, serde_json::to_vec(&spec).unwrap()).unwrap();
    GarminSynth::new(&edited).synthesize(&playback).unwrap();
    let s3 = run(&raw, &api).await;
    assert_eq!(s3.weigh_ins_pruned, 1, "{}", s3.line());
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM garmin_weigh_ins").await,
        3
    );
}
