//! A store written while what could not be stored sat in `dav_unstored`
//! and nothing listed what the server had opens under this build
//! listing every object it holds from a server, held at its etag, and
//! owing what the old table named: rung 1 of `schema_raw::LADDER`.

use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw as dr;
use datalib_etl_calendar::ingest::schema_raw::{full_ddl, IcsObjectRow};
use datalib_etl_calendar::ingest::{db_path_for, RawDb};
use datalib_etl_web::dav::state;

const OLD_UNSTORED_DDL: &str = "CREATE TABLE IF NOT EXISTS dav_unstored (
    collection TEXT NOT NULL, href TEXT NOT NULL, detail TEXT NOT NULL,
    PRIMARY KEY (collection, href))";

const ICS: &str =
    "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:tng-staff\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

async fn strings(pool: &sqlx::SqlitePool, sql: &'static str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(sql)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_the_listing_opens_holding_what_it_had_and_owing_the_rest() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        let mut ddl: Vec<String> = full_ddl()
            .into_iter()
            .filter(|d| !d.contains("dav_resources"))
            .collect();
        ddl.push(OLD_UNSTORED_DDL.to_string());
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = dr::open(&path, &ddl).await.unwrap();
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = pool.begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[
                IcsObjectRow::new(
                    "bridge",
                    "tng-staff",
                    Some("/c/bridge/staff.ics".into()),
                    Some("\"s1\"".into()),
                    ICS,
                ),
                // An `.ics` file's event: an href but no etag.
                IcsObjectRow::new("Bridge", "tng-riker", Some("Bridge.ics".into()), None, ICS),
            ],
            &now,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO dav_unstored (collection, href, detail) VALUES (?, ?, ?)")
            .bind("bridge")
            .bind("/c/bridge/reception.ics")
            .bind("the calendar listed this object, but did not return it when asked")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        dr::commit_run(&pool, "an earlier build").await.unwrap();
        pool.close().await;
    }

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let pool = db.pool();
    let listed: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT collection, href, etag FROM dav_resources ORDER BY href")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        listed,
        [
            ("bridge".into(), "/c/bridge/reception.ics".into(), None),
            (
                "bridge".into(),
                "/c/bridge/staff.ics".into(),
                Some("\"s1\"".into())
            ),
        ],
        "a server's objects are listed; a file's is not"
    );
    let held: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version \
         FROM dav_resources_bookkeeping ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        held,
        [
            (
                state::resource_id("bridge", "/c/bridge/reception.ics"),
                false,
                None
            ),
            (
                state::resource_id("bridge", "/c/bridge/staff.ics"),
                true,
                Some("\"s1\"".into())
            ),
        ]
    );
    let owed: Vec<String> = state::owed(pool, "bridge")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.href)
        .collect();
    assert_eq!(owed, ["/c/bridge/reception.ics"]);
    assert!(strings(
        pool,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'dav_unstored'"
    )
    .await
    .is_empty());
    assert_eq!(
        strings(
            pool,
            "SELECT value FROM _datalib_meta WHERE key = 'schema_version'"
        )
        .await,
        ["1"]
    );
    db.close().await;
}
