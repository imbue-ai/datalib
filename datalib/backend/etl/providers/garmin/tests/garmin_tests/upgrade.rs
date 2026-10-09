//! A store an earlier build wrote opens under this one with its rows
//! kept and what it held carried over. Two earlier shapes: before any
//! row said what it was held for, and the one that said it in a column
//! of the row (`fetched_on`, `listing_hash`), which the ladder's first
//! rung moves to the `_bookkeeping` sidecar.

use datalib_etl::doltlite_raw as dr;
use datalib_etl_garmin::ingest::{db_path_for, RawDb};
use datalib_etl_web::owed::{self, Listed};

const BEFORE_ANY_STAMP: &[&str] = &[
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

/// The shape with the stamps on the rows, and the sidecars as that
/// build wrote them, without `held_version`.
const STAMPS_ON_THE_ROWS: &[&str] = &[
    "CREATE TABLE garmin_daily (id TEXT PRIMARY KEY, payload TEXT NULL, \
     metric TEXT NOT NULL, calendar_date TEXT NOT NULL, fetched_on TEXT NULL)",
    "CREATE TABLE garmin_activities (id TEXT PRIMARY KEY, payload TEXT NULL, \
     start_time_gmt TEXT NULL, activity_type TEXT NULL, name TEXT NULL, listing_hash TEXT NULL)",
    "CREATE TABLE garmin_activity_details (id TEXT PRIMARY KEY, payload TEXT NULL, \
     listing_hash TEXT NULL)",
    "CREATE TABLE garmin_activity_files (id TEXT PRIMARY KEY, activity_id TEXT NOT NULL, \
     file_kind TEXT NOT NULL, blake3 TEXT NULL, listing_hash TEXT NULL, \
     CHECK (blake3 IS NULL OR length(blake3) = 64))",
    "CREATE TABLE garmin_wellness_files (id TEXT PRIMARY KEY, calendar_date TEXT NOT NULL, \
     file_kind TEXT NOT NULL, blake3 TEXT NULL, fetched_on TEXT NULL, \
     CHECK (blake3 IS NULL OR length(blake3) = 64))",
    "CREATE TABLE garmin_daily_bookkeeping (id TEXT PRIMARY KEY, fetched_at_utc TEXT NULL, \
     attempt_count INTEGER NOT NULL, last_attempt_at_utc TEXT NULL, last_error TEXT NULL, \
     volatile_payload TEXT NULL, tz_offset TEXT NULL)",
    "CREATE TABLE garmin_activity_details_bookkeeping (id TEXT PRIMARY KEY, \
     fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, last_attempt_at_utc TEXT NULL, \
     last_error TEXT NULL, volatile_payload TEXT NULL, tz_offset TEXT NULL)",
    "CREATE TABLE garmin_activity_files_bookkeeping (id TEXT PRIMARY KEY, \
     fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, last_attempt_at_utc TEXT NULL, \
     last_error TEXT NULL, volatile_payload TEXT NULL, tz_offset TEXT NULL)",
    "CREATE TABLE garmin_wellness_files_bookkeeping (id TEXT PRIMARY KEY, \
     fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, last_attempt_at_utc TEXT NULL, \
     last_error TEXT NULL, volatile_payload TEXT NULL, tz_offset TEXT NULL)",
];

const HASH: &str = "4d4f7265207468616e206f6e65204170706c652061206461792c20506963617264";

async fn earlier_store(
    dir: &tempfile::TempDir,
    shape: &[&str],
    rows: &[&str],
) -> std::path::PathBuf {
    let path = db_path_for(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = dr::open(&path, shape).await.unwrap();
    for sql in rows {
        // Audited: the test's own literals.
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&pool)
            .await
            .unwrap();
    }
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;
    path
}

async fn held(db: &RawDb, table: &'static str) -> Vec<(String, Option<String>)> {
    // Audited: `table` is a literal of the test.
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, held_version FROM {table}_bookkeeping ORDER BY id"
    )))
    .fetch_all(db.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_any_stamp_opens_with_its_rows_holding_nothing() {
    let d = tempfile::tempdir().unwrap();
    let path = earlier_store(
        &d,
        BEFORE_ANY_STAMP,
        &[
            "INSERT INTO garmin_daily VALUES ('sleep#2369-04-14', jsonb('null'), 'sleep', '2369-04-14')",
            "INSERT INTO garmin_activities VALUES ('17010413001', jsonb('{}'), NULL, NULL, 'Holodeck run')",
            "INSERT INTO garmin_activity_details VALUES ('17010413001', jsonb('{}'))",
            "INSERT INTO garmin_activity_files VALUES ('17010413001#fit', '17010413001', 'fit', NULL)",
            "INSERT INTO garmin_wellness_files VALUES ('2369-04-14#wellness_zip', '2369-04-14', 'wellness_zip', NULL)",
        ],
    )
    .await;

    let db = RawDb::open(&path).await.unwrap();
    for table in [
        "garmin_daily",
        "garmin_activities",
        "garmin_activity_details",
        "garmin_activity_files",
        "garmin_wellness_files",
    ] {
        // Audited: `table` is a literal of the list above.
        let rows: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(rows, 1, "{table} kept its row");
    }
    let listed = vec![Listed::new("sleep#2369-04-14", Some("2369-04-21"))];
    assert_eq!(
        owed::owed(db.pool(), "garmin_daily", listed.clone())
            .await
            .unwrap(),
        listed,
        "a row from before the stamps is held for nothing, and owed"
    );
    db.close().await;
}

/// The rung carries each stamp into the sidecar as it is, and drops the
/// column. A day fetched on the day it settled is held at that date and
/// not fetched again; a detail or file at its activity's listing version
/// likewise; a row the earlier build had not stamped holds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_with_the_stamps_on_its_rows_carries_them_into_the_sidecars() {
    let d = tempfile::tempdir().unwrap();
    let path = earlier_store(
        &d,
        STAMPS_ON_THE_ROWS,
        &[
            "INSERT INTO garmin_daily VALUES ('sleep#2369-04-07', jsonb('null'), 'sleep', '2369-04-07', '2369-04-14')",
            "INSERT INTO garmin_daily VALUES ('sleep#2369-04-14', jsonb('null'), 'sleep', '2369-04-14', NULL)",
            "INSERT INTO garmin_daily_bookkeeping (id, fetched_at_utc, attempt_count) \
             VALUES ('sleep#2369-04-07', '2369-04-14T09:00:00Z', 1), ('sleep#2369-04-14', NULL, 1)",
            &format!("INSERT INTO garmin_activities VALUES ('17010413001', jsonb('{{}}'), NULL, NULL, 'Holodeck run', '{HASH}')"),
            &format!("INSERT INTO garmin_activity_details VALUES ('17010413001', jsonb('{{}}'), '{HASH}')"),
            "INSERT INTO garmin_activity_details_bookkeeping (id, fetched_at_utc, attempt_count) \
             VALUES ('17010413001', '2369-04-14T09:00:00Z', 1)",
            &format!("INSERT INTO garmin_activity_files VALUES ('17010413001#fit', '17010413001', 'fit', NULL, '{HASH}')"),
            "INSERT INTO garmin_activity_files_bookkeeping (id, fetched_at_utc, attempt_count) \
             VALUES ('17010413001#fit', '2369-04-14T09:00:00Z', 1)",
            "INSERT INTO garmin_wellness_files VALUES ('2369-04-07#wellness_zip', '2369-04-07', 'wellness_zip', NULL, '2369-04-14')",
            "INSERT INTO garmin_wellness_files_bookkeeping (id, fetched_at_utc, attempt_count) \
             VALUES ('2369-04-07#wellness_zip', '2369-04-14T09:00:00Z', 1)",
        ],
    )
    .await;

    let db = RawDb::open(&path).await.unwrap();
    assert_eq!(
        held(&db, "garmin_daily").await,
        [
            (
                "sleep#2369-04-07".to_string(),
                Some("2369-04-14".to_string())
            ),
            ("sleep#2369-04-14".to_string(), None),
        ]
    );
    assert_eq!(
        held(&db, "garmin_activity_details").await,
        [("17010413001".to_string(), Some(HASH.to_string()))]
    );
    assert_eq!(
        held(&db, "garmin_activity_files").await,
        [("17010413001#fit".to_string(), Some(HASH.to_string()))]
    );
    assert_eq!(
        held(&db, "garmin_wellness_files").await,
        [(
            "2369-04-07#wellness_zip".to_string(),
            Some("2369-04-14".to_string())
        )]
    );
    for (table, column) in [
        ("garmin_daily", "fetched_on"),
        ("garmin_activity_details", "listing_hash"),
        ("garmin_activity_files", "listing_hash"),
        ("garmin_wellness_files", "fetched_on"),
    ] {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
            .bind(table)
            .bind(column)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0, "{table}.{column} is gone");
    }
    // With a week's refresh the 7th settled on the 14th, the day it was
    // fetched: held at its listed version, so not owed. The 14th was
    // never answered, so it is.
    let listed = vec![
        Listed::new("sleep#2369-04-07", Some("2369-04-14")),
        Listed::new("sleep#2369-04-14", Some("2369-04-21")),
    ];
    assert_eq!(
        owed::owed(db.pool(), "garmin_daily", listed).await.unwrap(),
        [Listed::new("sleep#2369-04-14", Some("2369-04-21"))]
    );
    assert!(owed::owed(
        db.pool(),
        "garmin_activity_details",
        vec![Listed::new("17010413001", Some(HASH))]
    )
    .await
    .unwrap()
    .is_empty());
    db.close().await;
}
