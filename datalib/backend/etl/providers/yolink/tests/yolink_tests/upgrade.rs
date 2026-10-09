//! A store an earlier build wrote opens under this one with its rows
//! kept and what it had walked carried over. The earlier shape resumed
//! each device from `yolink_devices.last_ts_ms` and kept the windows
//! that failed behind it in `yolink_windows`; the ladder's first rung
//! turns the two into coverage spans and drops them.

use chrono::{TimeZone, Utc};
use datalib_etl::doltlite_raw as dr;
use datalib_etl::doltlite_raw::WirePayloadRow;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_yolink::ingest::schema_raw::{device_scope, span_end, YolinkReadingRow};
use datalib_etl_yolink::ingest::{db_path_for, RawDb};

const DAY: i64 = 86_400_000;
const FIVE_MIN: i64 = 300_000;

fn day(n: i64) -> i64 {
    Utc.with_ymd_and_hms(2369, 4, 1, 0, 0, 0)
        .unwrap()
        .timestamp_millis()
        + n * DAY
}

/// The earlier build's shape: the readings table and the sidecars as
/// they still are, the devices table with its cursor, and the table of
/// failed windows.
fn with_the_cursor() -> Vec<String> {
    vec![
        "CREATE TABLE yolink_devices (id TEXT PRIMARY KEY, family_device_id TEXT NOT NULL, \
         kind TEXT NOT NULL, start_ms INTEGER NOT NULL, last_ts_ms INTEGER NULL)"
            .to_string(),
        YolinkReadingRow::ddl(),
        "CREATE TABLE yolink_windows (id TEXT PRIMARY KEY, device_name TEXT NOT NULL, \
         start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL)"
            .to_string(),
        dr::bookkeeping_ddl_for("yolink_devices"),
        dr::bookkeeping_ddl_for("yolink_readings"),
        dr::bookkeeping_ddl_for("yolink_windows"),
    ]
}

async fn earlier_store(dir: &tempfile::TempDir, rows: &[String]) -> std::path::PathBuf {
    let path = db_path_for(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let shape = with_the_cursor();
    let shape: Vec<&str> = shape.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &shape).await.unwrap();
    for sql in rows {
        // Audited: the test's own literals.
        sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    dr::record_object_error(
        &mut tx,
        "yolink_windows",
        &format!("warp-core-coolant#{}#{}", day(7), day(14) + FIVE_MIN),
        "connection reset by peer",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;
    path
}

async fn has_column(pool: &sqlx::SqlitePool, table: &str, column: &str) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap();
    n > 0
}

/// Three devices as the earlier build left them: one walked to its
/// cursor with a window that failed on the way, one whose `start` was
/// widened by a run that never satisfied the config (the record still
/// names the old start), and one with no reading yet. The rung records
/// what each had walked, so the next run asks for the failed window,
/// the widened stretch and the time since, and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_with_the_cursor_opens_with_what_it_walked_as_coverage() {
    let d = tempfile::tempdir().unwrap();
    let cursor = day(20) + DAY / 2;
    let path = earlier_store(
        &d,
        &[
            format!(
                "INSERT INTO yolink_devices VALUES \
                 ('warp-core-coolant', 'a', 'watermeter', {}, {cursor}), \
                 ('cargo-bay-2', 'b', 'watermeter', {}, {cursor}), \
                 ('holodeck-3', 'c', 'watermeter', {}, NULL)",
                day(0),
                day(0),
                day(0)
            ),
            format!(
                "INSERT INTO yolink_readings VALUES \
                 ('warp-core-coolant#{cursor}#water_meter_gal', NULL, 'warp-core-coolant', \
                 {cursor}, 'water_meter_gal', 20.0)"
            ),
            format!(
                "INSERT INTO yolink_windows VALUES \
                 ('warp-core-coolant#{}#{}', 'warp-core-coolant', {}, {})",
                day(7),
                day(14) + FIVE_MIN,
                day(7),
                day(14) + FIVE_MIN
            ),
            "CREATE TABLE IF NOT EXISTS sync_scope_config (scope TEXT PRIMARY KEY, config TEXT NOT NULL, updated_at_utc TEXT NOT NULL, tz_offset TEXT NULL)".to_string(),
            "INSERT INTO sync_scope_config (scope, config, updated_at_utc, tz_offset) VALUES \
             ('yolink:download', '{\"device_starts\":{\"cargo-bay-2\":\"2369-04-08\"}}', \
             '2369-04-20T12:00:00Z', '+00:00')"
                .to_string(),
        ],
    )
    .await;

    let db = RawDb::open(&path).await.unwrap();
    let pool = db.pool();
    assert_eq!(
        coverage::held(pool, &device_scope("warp-core-coolant"))
            .await
            .unwrap(),
        [
            Span::new(span_end(day(0)), span_end(day(7))),
            Span::new(span_end(day(14) + FIVE_MIN), span_end(cursor)),
        ],
        "from the start to the cursor, less the window that failed"
    );
    assert_eq!(
        coverage::held(pool, &device_scope("cargo-bay-2"))
            .await
            .unwrap(),
        [Span::new(span_end(day(7)), span_end(cursor))],
        "from the start the cursor was walked under, not the widened one"
    );
    assert!(
        coverage::held(pool, &device_scope("holodeck-3"))
            .await
            .unwrap()
            .is_empty(),
        "a device with no reading had walked nothing it can vouch for"
    );
    let want = Span::new(span_end(day(0)), span_end(day(21)));
    let held = coverage::held(pool, &device_scope("warp-core-coolant"))
        .await
        .unwrap();
    assert_eq!(
        coverage::gaps(&want, &held),
        [
            Span::new(span_end(day(7)), span_end(day(14) + FIVE_MIN)),
            Span::new(span_end(cursor), span_end(day(21))),
        ],
        "the next run owes the failed window and the time since the cursor"
    );

    let readings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM yolink_readings")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(readings, 1, "the readings are kept");
    assert!(!has_column(pool, "yolink_devices", "last_ts_ms").await);
    assert!(!has_column(pool, "yolink_windows", "id").await);
    assert!(!has_column(pool, "yolink_windows_bookkeeping", "id").await);
    let windows_problems: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM problems WHERE scope_key LIKE 'yolink_windows:%'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        windows_problems, 0,
        "a failed window's row went with its table"
    );
    let retired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sync_scope_config'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(retired, 0, "the record of the config went with its table");
    db.close().await;

    let again = RawDb::open(&path).await.unwrap();
    assert_eq!(
        coverage::held(again.pool(), &device_scope("cargo-bay-2"))
            .await
            .unwrap()
            .len(),
        1,
        "a second open runs no rung again"
    );
    again.close().await;
}
