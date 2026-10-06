//! A store written before days carried the date they were fetched on,
//! and before details and files carried the listing version they were
//! fetched for, opens under this build with its rows kept: every change
//! to the shape is a column added.

use datalib_etl::doltlite_raw as dr;
use datalib_etl_garmin::ingest::{db_path_for, RawDb};

const EARLIER_SHAPE: &[&str] = &[
    "CREATE TABLE garmin_daily (id TEXT PRIMARY KEY, payload TEXT NULL, \
     metric TEXT NOT NULL, calendar_date TEXT NOT NULL)",
    "CREATE TABLE garmin_activities (id TEXT PRIMARY KEY, payload TEXT NULL, \
     start_time_gmt TEXT NULL, activity_type TEXT NULL, name TEXT NULL)",
    "CREATE TABLE garmin_activity_details (id TEXT PRIMARY KEY, payload TEXT NULL)",
    "CREATE TABLE garmin_activity_files (id TEXT PRIMARY KEY, activity_id TEXT NOT NULL, \
     file_kind TEXT NOT NULL, blake3 TEXT NULL, \
     CHECK (blake3 IS NULL OR length(blake3) = 64))",
    "CREATE TABLE garmin_wellness_files (id TEXT PRIMARY KEY, calendar_date TEXT NOT NULL, \
     file_kind TEXT NOT NULL, blake3 TEXT NULL, \
     CHECK (blake3 IS NULL OR length(blake3) = 64))",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_the_held_stamps_opens_with_its_rows() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = dr::open(&path, EARLIER_SHAPE).await.unwrap();
    for sql in [
        "INSERT INTO garmin_daily VALUES ('sleep#2369-04-14', jsonb('null'), 'sleep', '2369-04-14')",
        "INSERT INTO garmin_activities VALUES ('17010413001', jsonb('{}'), NULL, NULL, 'Holodeck run')",
        "INSERT INTO garmin_activity_details VALUES ('17010413001', jsonb('{}'))",
        "INSERT INTO garmin_activity_files VALUES ('17010413001#fit', '17010413001', 'fit', NULL)",
        "INSERT INTO garmin_wellness_files VALUES ('2369-04-14#wellness_zip', '2369-04-14', 'wellness_zip', NULL)",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.unwrap();
    for (table, stamp) in [
        ("garmin_daily", "fetched_on"),
        ("garmin_activities", "listing_hash"),
        ("garmin_activity_details", "listing_hash"),
        ("garmin_activity_files", "listing_hash"),
        ("garmin_wellness_files", "fetched_on"),
    ] {
        // Audited: both names are literals from the list above.
        let unstamped: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE {stamp} IS NULL"
        )))
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            unstamped, 1,
            "{table} kept its row, not yet held for anything"
        );
    }
    db.close().await;
}
